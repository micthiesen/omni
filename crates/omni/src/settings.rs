//! Settings and secrets kept by Omni ("Settings" in `docs/architecture.md`).
//!
//! Every variable `Config` decodes can be stored in the docstore
//! (`omni-setting` rows) and changed on the `/settings` page by a trusted
//! browser, so a key rotation or a new URL needs neither SSH nor an edit of
//! `.env`. Secret values (keys `omni_config::is_sensitive_key` matches, and
//! a few private ones) are sealed with `settings.key` in the data directory
//! (AES-256-GCM, bound to the key name) and never shown again; other values
//! are shown only to a trusted browser. No value is ever logged.
//!
//! Precedence: a stored value wins, the environment is the fallback, then
//! the built-in default. At boot, before anything else reads the
//! configuration, the stored values are laid over the environment; a stored
//! set that does not validate is ignored for that boot (and said so at
//! ERROR), so a bad value can never keep Omni down. Every environment value
//! is copied into the store once (the migration), after which `.env` entries
//! can be removed. Saved values apply at the next start, which the page can
//! trigger (`POST /api/settings/restart`): the container's restart policy
//! starts it again.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::OnceLock;

use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse as _, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use omni_api::settings::{SettingSource, SettingUpdate, SettingView, SettingsView};
use omni_config::{CONFIG_KEYS, Config};
use omni_mcp::gateway::secrets::GatewaySecrets;
use omni_runtime::{AppContext, BootError, BootPhase, BootStep, Subsystem};
use omni_server_kit::{ApiError, JsonBody};
use omni_store::cbor::Extra;
use omni_store::entity::{Entity, EntityOps as _, EntityWrite as _, UpsertOpts};
use omni_store::{Store, StoreError};
use serde::{Deserialize, Serialize};

const LOG: &str = "Main:Settings";
pub const KEY_FILE: &str = "settings.key";
/// Environment-only: what Omni needs before it can read its settings (where
/// the store is, how it was started, the port it listens on), and paths the
/// image sets, which a stored copy would pin across image changes.
const BOOTSTRAP_KEYS: [&str; 4] = [
    "DB_NAME",
    "DOCKERIZED",
    "FRONTEND_PORT",
    "OMNI_RECIPES_WEB_DIST",
];
/// Not matched by the sensitive-name patterns but still credentials or
/// personal identifiers.
const EXTRA_SECRET_KEYS: [&str; 9] = [
    "PUSHOVER_USER",
    "ICLOUD_USERNAME",
    "ICLOUD_REMINDERS_ACCOUNT",
    "EMAIL_SELF_ADDRESS",
    "SMTP_USER",
    "EMAIL_FROM",
    "LOGS_EMAIL_TO",
    "CLAUDE_NOTIFY_TARGETS",
    "WHISKER_CREDENTIALS",
];
const VALUE_LIMIT: usize = 16 * 1024;

/// `omni-setting`: one stored configuration value.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingRow {
    pub key: String,
    /// A non-secret value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// A secret value, sealed with `settings.key`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sealed: Option<String>,
    pub updated_at: i64,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Entity for SettingRow {
    const NAME: &'static str = "omni-setting";
    type Key = String;
    fn key(&self) -> String {
        self.key.clone()
    }
}

/// `omni-setting-import`: the environment value of `key` was copied into the
/// store once; a value removed on the page is not copied again.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingImport {
    pub key: String,
    pub imported_at: i64,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Entity for SettingImport {
    const NAME: &'static str = "omni-setting-import";
    type Key = String;
    fn key(&self) -> String {
        self.key.clone()
    }
}

pub fn entities() -> Vec<omni_store::entity::EntityDescriptor> {
    vec![
        omni_store::entity::EntityDescriptor::of::<SettingRow>(),
        omni_store::entity::EntityDescriptor::of::<SettingImport>(),
    ]
}

pub fn is_secret(key: &str) -> bool {
    omni_config::is_sensitive_key(key) || EXTRA_SECRET_KEYS.contains(&key)
}

pub fn is_editable(key: &str) -> bool {
    CONFIG_KEYS.contains(&key) && !BOOTSTRAP_KEYS.contains(&key)
}

fn context(key: &str) -> String {
    format!("setting:{key}")
}

/// The section a key is listed under.
pub fn group(key: &str) -> &'static str {
    const GROUPS: [(&str, &str); 26] = [
        ("PUSHOVER_", "Notifications"),
        ("IOS_CONTROL_", "iOS app"),
        ("LIVESTREAM_", "Livestreams"),
        ("KICK_", "Livestreams"),
        ("OFFLINE_", "Livestreams"),
        ("ICLOUD_REMINDERS_", "Reminders"),
        ("ICLOUD_", "Email and calendar"),
        ("EMAIL_", "Email and calendar"),
        ("SMTP_", "Email and calendar"),
        ("LOGS_EMAIL", "Email and calendar"),
        ("CALENDAR_", "Email and calendar"),
        ("PARCEL_", "Parcels"),
        ("PRESSPODS_", "PressPods"),
        ("ELEVENLABS_", "PressPods"),
        ("JINA_", "PressPods"),
        ("KARAKEEP_", "PressPods"),
        ("RECS_", "Media"),
        ("TASTE_", "Media"),
        ("PODCAST", "Podcasts"),
        ("CASTRO_", "Podcasts"),
        ("YOUTUBE_", "YouTube"),
        ("OMNI_GATEWAY", "MCP gateway"),
        ("RECIPES_", "Recipes"),
        ("CLAUDE_", "Claude Code"),
        ("OMNI_DEVICE_LINK", "Claude Code"),
        ("WHISKER_", "Pets"),
    ];
    const MEDIA: [&str; 10] = [
        "PLEX_",
        "RADARR_",
        "SONARR_",
        "TMDB_",
        "OBSERVER_",
        "NZBGET_",
        "PROWLARR_",
        "DELUGE_",
        "AUDIOBOOK",
        "ARR_",
    ];
    if MEDIA.iter().any(|p| key.starts_with(p)) {
        return "Media";
    }
    if key.ends_with("_MODEL") || key.ends_with("_API_KEY") {
        return "Models and APIs";
    }
    GROUPS
        .iter()
        .find(|(prefix, _)| key.starts_with(prefix))
        .map_or("General", |(_, group)| group)
}

/// Stored values as plain text (secrets opened), read before the app is
/// built from the store `DB_NAME` names and the key beside it (as
/// `app_paths` places the data directory). Empty when either is missing.
pub async fn stored_values(env: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let Ok(config) = Config::from_env(env) else {
        return BTreeMap::new();
    };
    let db = config.db_path();
    let dir = db
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| std::path::PathBuf::from("."), Path::to_path_buf);
    read_stored(&db, &dir).await
}

/// [`stored_values`] for an explicit store file and data directory.
pub async fn read_stored(db: &Path, data_dir: &Path) -> BTreeMap<String, String> {
    if !db.is_file() {
        return BTreeMap::new();
    }
    let clock: omni_core::clock::SharedClock = Arc::new(omni_core::clock::SystemClock);
    let Ok(store) = Store::open(db, omni_store::StoreOptions::new(clock)).await else {
        return BTreeMap::new();
    };
    let rows = store
        .read(|docs| docs.get_all::<SettingRow>())
        .await
        .unwrap_or_default();
    let secrets = GatewaySecrets::load_or_create(&data_dir.join(KEY_FILE)).ok();
    let mut values = BTreeMap::new();
    for row in rows {
        if !is_editable(&row.key) {
            continue;
        }
        let value = match (&row.value, &row.sealed, &secrets) {
            (Some(value), _, _) => Some(value.clone()),
            (None, Some(sealed), Some(secrets)) => secrets.open(&context(&row.key), sealed),
            _ => None,
        };
        if let Some(value) = value {
            values.insert(row.key, value);
        }
    }
    values
}

/// A validation failure that never quotes a secret: a reason may include
/// the value it refused.
pub fn safe_reason(error: &omni_config::ConfigError) -> String {
    let key = match error {
        omni_config::ConfigError::Invalid { key, .. }
        | omni_config::ConfigError::WeakToken(key) => *key,
        omni_config::ConfigError::EmailFrom(_) => "EMAIL_FROM",
    };
    if is_secret(key) {
        format!("{key} is not valid")
    } else {
        error.to_string()
    }
}

/// The environment with stored values laid over it, and whether the stored
/// set validated. A stored set that does not is ignored for this boot.
pub async fn effective_environment() -> (BTreeMap<String, String>, Result<(), String>) {
    let env: BTreeMap<String, String> = std::env::vars().collect();
    let stored = stored_values(&env).await;
    if stored.is_empty() {
        return (env, Ok(()));
    }
    let mut merged = env.clone();
    merged.extend(stored);
    match Config::from_env(&merged) {
        Ok(_) => (merged, Ok(())),
        Err(error) => (env, Err(safe_reason(&error))),
    }
}

/// The settings service: what booted, the store and the key.
#[derive(Clone)]
pub struct Settings {
    store: Store,
    clock: omni_core::clock::SharedClock,
    secrets: GatewaySecrets,
    /// The environment Omni booted with (stored values laid over it).
    booted: Arc<BTreeMap<String, String>>,
    /// The container environment alone.
    environment: Arc<BTreeMap<String, String>>,
    owner: omni_runtime::owner::OwnerTrust,
    shutdown: tokio_util::sync::CancellationToken,
    tracker: tokio_util::task::TaskTracker,
    /// In its container, where the restart policy starts it again.
    dockerized: bool,
    booted_at: i64,
}

impl Settings {
    pub fn new(
        ctx: &AppContext,
        booted: BTreeMap<String, String>,
    ) -> Result<Self, omni_mcp::gateway::secrets::SecretsError> {
        Self::with_environment(ctx, booted, std::env::vars().collect())
    }

    /// Over an explicit container environment (tests).
    pub fn with_environment(
        ctx: &AppContext,
        booted: BTreeMap<String, String>,
        environment: BTreeMap<String, String>,
    ) -> Result<Self, omni_mcp::gateway::secrets::SecretsError> {
        std::fs::create_dir_all(&ctx.paths.data_dir).map_err(|e| {
            omni_mcp::gateway::secrets::SecretsError::KeyFile {
                path: ctx.paths.data_dir.clone(),
                reason: e.to_string(),
            }
        })?;
        let secrets = GatewaySecrets::load_or_create(&ctx.paths.data_dir.join(KEY_FILE))?;
        Ok(Self {
            store: ctx.store.clone(),
            clock: ctx.clock.clone(),
            secrets,
            booted: Arc::new(booted),
            environment: Arc::new(environment),
            owner: ctx.owner.clone(),
            shutdown: ctx.shutdown.clone(),
            tracker: ctx.tracker.clone(),
            dockerized: ctx.config.dockerized,
            booted_at: ctx.clock.now_ms(),
        })
    }

    fn row(&self, key: &str, value: &str, now: i64) -> SettingRow {
        let (value, sealed) = if is_secret(key) {
            (None, Some(self.secrets.seal(&context(key), value)))
        } else {
            (Some(value.to_owned()), None)
        };
        SettingRow {
            key: key.to_owned(),
            value,
            sealed,
            updated_at: now,
            extra: Extra::default(),
        }
    }

    fn open(&self, row: &SettingRow) -> Option<String> {
        match (&row.value, &row.sealed) {
            (Some(value), _) => Some(value.clone()),
            (None, Some(sealed)) => self.secrets.open(&context(&row.key), sealed),
            _ => None,
        }
    }

    /// Copies every environment value into the store once (the migration
    /// from `.env`); how many.
    pub async fn import_environment(&self) -> Result<usize, StoreError> {
        let now = self.clock.now_ms();
        let candidates: Vec<SettingRow> = self
            .environment
            .iter()
            .filter(|(key, value)| is_editable(key) && !value.is_empty())
            .map(|(key, value)| self.row(key, value, now))
            .collect();
        self.store
            .write(move |tx| {
                let mut imported = 0;
                for row in candidates {
                    if tx.get::<SettingImport>(&row.key)?.is_some() {
                        continue;
                    }
                    if tx.get::<SettingRow>(&row.key)?.is_none() {
                        tx.upsert(&row, UpsertOpts::default())?;
                        imported += 1;
                    }
                    tx.upsert(
                        &SettingImport {
                            key: row.key.clone(),
                            imported_at: now,
                            extra: Extra::default(),
                        },
                        UpsertOpts::default(),
                    )?;
                }
                Ok::<_, StoreError>(imported)
            })
            .await
    }

    async fn rows(&self) -> Result<BTreeMap<String, SettingRow>, StoreError> {
        Ok(self
            .store
            .read(|docs| docs.get_all::<SettingRow>())
            .await?
            .into_iter()
            .map(|row| (row.key.clone(), row))
            .collect())
    }

    pub async fn view(&self, trusted: bool) -> Result<SettingsView, StoreError> {
        let rows = self.rows().await?;
        let mut settings = Vec::new();
        for key in CONFIG_KEYS {
            let row = rows.get(*key);
            let stored = row.and_then(|row| self.open(row));
            let env = self.environment.get(*key).filter(|v| !v.is_empty());
            let effective = stored.clone().or_else(|| env.cloned());
            let booted = self.booted.get(*key).filter(|v| !v.is_empty());
            let secret = is_secret(key);
            settings.push(SettingView {
                key: (*key).to_owned(),
                group: group(key).to_owned(),
                secret,
                editable: is_editable(key),
                source: if stored.is_some() {
                    SettingSource::Stored
                } else if env.is_some() {
                    SettingSource::Environment
                } else {
                    SettingSource::Default
                },
                set: effective.is_some(),
                value: if secret || !trusted {
                    None
                } else {
                    effective.clone()
                },
                environment_set: env.is_some(),
                environment_differs: stored.is_some() && env.is_some() && stored.as_ref() != env,
                restart_needed: effective.as_ref() != booted,
                updated_at: row.map(|row| row.updated_at),
            });
        }
        let restart_needed = settings.iter().any(|s| s.restart_needed);
        Ok(SettingsView {
            settings,
            restart_needed,
            values_hidden: !trusted,
            booted_at: self.booted_at,
        })
    }

    /// The environment as the next start would see it, with `key` changed.
    async fn candidate(
        &self,
        key: &str,
        value: Option<&str>,
    ) -> Result<BTreeMap<String, String>, StoreError> {
        let mut env = (*self.environment).clone();
        for (k, row) in self.rows().await? {
            if let Some(v) = self.open(&row) {
                env.insert(k, v);
            }
        }
        match value {
            Some(value) => {
                env.insert(key.to_owned(), value.to_owned());
            }
            None => {
                env.remove(key);
                if let Some(v) = self.environment.get(key) {
                    env.insert(key.to_owned(), v.clone());
                }
            }
        }
        Ok(env)
    }

    /// Saves `value` for `key` after the whole configuration validates with it.
    pub async fn set(&self, key: &str, value: &str) -> Result<(), SettingError> {
        if !is_editable(key) {
            return Err(SettingError::NotEditable(key.to_owned()));
        }
        if value.is_empty() {
            return Err(SettingError::Invalid(format!(
                "{key} needs a value; to remove the stored one, use the environment value instead"
            )));
        }
        if value.len() > VALUE_LIMIT || value.chars().any(|c| c.is_control() && c != '\n') {
            return Err(SettingError::Invalid(format!(
                "{key} must be at most 16 KiB of text without control characters"
            )));
        }
        let env = self.candidate(key, Some(value)).await?;
        if let Err(error) = Config::from_env(&env) {
            return Err(SettingError::Invalid(safe_reason(&error)));
        }
        if omni_config::is_sensitive_key(key) {
            omni_core::secrets::register_runtime(value);
        }
        let row = self.row(key, value, self.clock.now_ms());
        self.store
            .write(move |tx| tx.upsert(&row, UpsertOpts::default()))
            .await?;
        tracing::info!(target: LOG, setting = %key, "Setting saved; it applies at the next start");
        Ok(())
    }

    /// Removes the stored value: the environment value or default applies.
    pub async fn clear(&self, key: &str) -> Result<(), SettingError> {
        if !is_editable(key) {
            return Err(SettingError::NotEditable(key.to_owned()));
        }
        let env = self.candidate(key, None).await?;
        if let Err(error) = Config::from_env(&env) {
            return Err(SettingError::Invalid(safe_reason(&error)));
        }
        let owned = key.to_owned();
        self.store
            .write(move |tx| tx.delete::<SettingRow>(&owned).map(|_| ()))
            .await?;
        tracing::info!(target: LOG, setting = %key, "Stored setting removed; it applies at the next start");
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SettingError {
    #[error("{0} cannot be changed here: Omni needs it before it can read its settings")]
    NotEditable(String),
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

fn setting_error(error: SettingError) -> Response {
    match error {
        SettingError::NotEditable(_) | SettingError::Invalid(_) => {
            ApiError::bad_request(error.to_string()).into_response()
        }
        SettingError::Store(error) => ApiError::internal(error).into_response(),
    }
}

async fn list(State(settings): State<Settings>, headers: HeaderMap) -> Response {
    let trusted = settings.owner.is_trusted(&headers).await;
    match settings.view(trusted).await {
        Ok(view) => Json(view).into_response(),
        Err(error) => ApiError::internal(error).into_response(),
    }
}

async fn one(settings: &Settings, key: &str) -> Response {
    match settings.view(true).await {
        Ok(view) => match view.settings.into_iter().find(|s| s.key == key) {
            Some(mut setting) => {
                if setting.secret {
                    setting.value = None;
                }
                Json(setting).into_response()
            }
            None => ApiError::not_found(format!("No setting {key}")).into_response(),
        },
        Err(error) => ApiError::internal(error).into_response(),
    }
}

async fn save(
    State(settings): State<Settings>,
    UrlPath(key): UrlPath<String>,
    JsonBody(body): JsonBody<SettingUpdate>,
) -> Response {
    match settings.set(&key, &body.value).await {
        Ok(()) => one(&settings, &key).await,
        Err(error) => setting_error(error),
    }
}

async fn remove(State(settings): State<Settings>, UrlPath(key): UrlPath<String>) -> Response {
    match settings.clear(&key).await {
        Ok(()) => one(&settings, &key).await,
        Err(error) => setting_error(error),
    }
}

/// Ends the process after answering; the container's restart policy starts
/// it again with the saved values.
async fn restart(State(settings): State<Settings>) -> Response {
    if !settings.dockerized {
        return ApiError::conflict(
            "Omni is not running in its container, so nothing would start it again; restart it yourself",
        )
        .into_response();
    }
    tracing::warn!(target: LOG, "Restart requested from the settings page");
    let shutdown = settings.shutdown.clone();
    // After the answer is on its way.
    omni_core::spawn::spawn_tracked(&settings.tracker, "settings-restart", async move {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        shutdown.cancel();
    });
    StatusCode::ACCEPTED.into_response()
}

pub fn router(settings: Settings) -> Router {
    let guarded = Router::new()
        .route("/api/settings/{key}", put(save).delete(remove))
        .route("/api/settings/restart", post(restart))
        .route_layer(axum::middleware::from_fn_with_state(
            settings.owner.clone(),
            omni_runtime::owner::require_trusted,
        ))
        .with_state(settings.clone());
    Router::new()
        .route("/api/settings", get(list))
        .with_state(settings)
        .merge(guarded)
}

/// The environment this process booted with (set once by `app::with_app`).
static BOOTED: OnceLock<BTreeMap<String, String>> = OnceLock::new();

pub fn set_booted(env: BTreeMap<String, String>) {
    let _ = BOOTED.set(env);
}

/// The settings routes and the boot migration from the environment.
pub fn subsystem(ctx: &AppContext) -> Result<Subsystem, String> {
    let booted = BOOTED
        .get()
        .cloned()
        .unwrap_or_else(|| std::env::vars().collect());
    let settings = Settings::new(ctx, booted).map_err(|e| e.to_string())?;
    let mut subsystem = Subsystem::named("settings");
    subsystem.router = router(settings.clone());
    subsystem.entities = entities();
    subsystem.boot_steps.push(BootStep {
        phase: BootPhase::Migrate,
        name: "importSettings",
        run: Box::new(move |_ctx: AppContext| {
            let settings = settings.clone();
            Box::pin(async move {
                let imported = settings
                    .import_environment()
                    .await
                    .map_err(|e| BootError::new("importSettings", e.to_string()))?;
                if imported > 0 {
                    tracing::info!(target: LOG, "Copied {imported} environment setting(s) into Omni's store");
                }
                Ok(())
            })
        }),
    });
    Ok(subsystem)
}
