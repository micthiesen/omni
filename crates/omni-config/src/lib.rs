//! Typed environment configuration.
//!
//! `Config` has one typed field per variable. Derived fallbacks (Pushover
//! channel tokens, `EMAIL_SELF_ADDRESS`) are methods so the raw value stays
//! visible. `ModelRole` and `PushoverChannel` live here (not in `omni-ai` /
//! `omni-alerts`) because `Config` must name them and those crates depend on
//! this one; both are re-exported from their original homes.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::LazyLock;

use omni_core::LogLevel;
use serde::{Deserialize, Serialize};

/// The only accepted non-empty `EMAIL_FROM`.
pub const OUTGOING_EMAIL_FROM: &str = "michael@thiesen.dev";

/// Minimum length of MCP / device-link bearer tokens.
pub const MIN_MCP_TOKEN_LENGTH: usize = 32;
const MIN_DISTINCT_TOKEN_CHARS: usize = 12;

/// Pushover applications. Every channel but `General` falls back to `PUSHOVER_TOKEN`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PushoverChannel {
    General,
    Live,
    Calendar,
    Recs,
    Podcast,
    PressPods,
}

/// Language-model call sites; each has a code default and maybe an env override.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ModelRole {
    Extraction,
    CalendarExtraction,
    Triage,
    RecsShortlist,
    RecsSelection,
    TasteReflection,
    PodcastTasteReflection,
    LivestreamIntelligence,
    PressPodsMetadata,
    PressPodsCleaning,
    ObserverRepair,
    ArrRecovery,
}

impl ModelRole {
    /// Code default. Production deploys use these.
    pub fn default_model(self) -> &'static str {
        match self {
            ModelRole::CalendarExtraction
            | ModelRole::RecsSelection
            | ModelRole::PressPodsCleaning => "openai:gpt-6-sol",
            ModelRole::Extraction
            | ModelRole::Triage
            | ModelRole::RecsShortlist
            | ModelRole::TasteReflection
            | ModelRole::PodcastTasteReflection
            | ModelRole::LivestreamIntelligence
            | ModelRole::PressPodsMetadata
            | ModelRole::ObserverRepair
            | ModelRole::ArrRecovery => "openai:gpt-6-luna",
        }
    }

    /// The environment variable that overrides the default, if any.
    pub fn env_key(self) -> Option<&'static str> {
        match self {
            ModelRole::Extraction => Some("EXTRACTION_MODEL"),
            ModelRole::CalendarExtraction => Some("CALENDAR_EXTRACTION_MODEL"),
            ModelRole::Triage => Some("TRIAGE_MODEL"),
            ModelRole::RecsShortlist => Some("RECS_SHORTLIST_MODEL"),
            ModelRole::RecsSelection => Some("RECS_SELECTION_MODEL"),
            ModelRole::TasteReflection => Some("TASTE_REFLECTION_MODEL"),
            ModelRole::PodcastTasteReflection => Some("PODCAST_TASTE_REFLECTION_MODEL"),
            ModelRole::LivestreamIntelligence => Some("LIVESTREAM_INTELLIGENCE_MODEL"),
            ModelRole::PressPodsMetadata => Some("PRESSPODS_METADATA_MODEL"),
            ModelRole::PressPodsCleaning => Some("PRESSPODS_CLEANING_MODEL"),
            ModelRole::ObserverRepair | ModelRole::ArrRecovery => None,
        }
    }
}

/// `PRESSPODS_TTS_PROVIDER`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TtsProvider {
    Higgs,
    Elevenlabs,
}

/// `WHISKER_CREDENTIALS` split at the first `:`.
#[derive(Clone, PartialEq, Eq)]
pub struct WhiskerCredentials {
    pub email: String,
    pub password: String,
}

impl fmt::Debug for WhiskerCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WhiskerCredentials(***)")
    }
}

/// Renders one field for the redacted config summary.
trait SummaryValue {
    fn summary(&self) -> String;
}

impl SummaryValue for String {
    fn summary(&self) -> String {
        self.clone()
    }
}
impl SummaryValue for bool {
    fn summary(&self) -> String {
        self.to_string()
    }
}
impl SummaryValue for f64 {
    fn summary(&self) -> String {
        omni_core::js::number_to_string(*self)
    }
}
impl SummaryValue for u32 {
    fn summary(&self) -> String {
        self.to_string()
    }
}
impl SummaryValue for u64 {
    fn summary(&self) -> String {
        self.to_string()
    }
}
impl SummaryValue for LogLevel {
    fn summary(&self) -> String {
        self.as_str().to_owned()
    }
}
impl SummaryValue for TtsProvider {
    fn summary(&self) -> String {
        match self {
            TtsProvider::Higgs => "higgs".to_owned(),
            TtsProvider::Elevenlabs => "elevenlabs".to_owned(),
        }
    }
}
impl SummaryValue for WhiskerCredentials {
    fn summary(&self) -> String {
        "***".to_owned()
    }
}
impl<T: SummaryValue> SummaryValue for Option<T> {
    fn summary(&self) -> String {
        match self {
            Some(value) => value.summary(),
            None => "undefined".to_owned(),
        }
    }
}

macro_rules! config_struct {
    ($( $(#[$meta:meta])* $field:ident : $ty:ty = $key:literal ),* $(,)?) => {
        /// Decoded configuration; construct with [`Config::from_env`].
        ///
        /// `Debug` is redacted (it prints [`Config::redacted_summary`]).
        #[derive(Clone, PartialEq)]
        pub struct Config {
            $( $(#[$meta])* #[doc = concat!("`", $key, "`")] pub $field: $ty, )*
        }

        /// Every variable `Config` decodes, in declaration order.
        pub const CONFIG_KEYS: &[&str] = &[$($key),*];

        impl Config {
            fn entries(&self) -> Vec<(&'static str, String)> {
                vec![$( ($key, SummaryValue::summary(&self.$field)) ),*]
            }
        }
    };
}

config_struct! {
    log_level: LogLevel = "LOG_LEVEL",
    pushover_user: Option<String> = "PUSHOVER_USER",
    pushover_token: Option<String> = "PUSHOVER_TOKEN",
    dockerized: bool = "DOCKERIZED",
    db_name: String = "DB_NAME",
    kick_client_id: Option<String> = "KICK_CLIENT_ID",
    kick_client_secret: Option<String> = "KICK_CLIENT_SECRET",
    offline_notifications: bool = "OFFLINE_NOTIFICATIONS",
    /// Raw value; use [`Config::pushover_token_for`] for the derived fallback.
    pushover_live_token: Option<String> = "PUSHOVER_LIVE_TOKEN",
    livestream_intelligence_enabled: bool = "LIVESTREAM_INTELLIGENCE_ENABLED",
    /// Only `openai:gpt-6-luna` is accepted.
    livestream_intelligence_model: Option<String> = "LIVESTREAM_INTELLIGENCE_MODEL",
    livestream_monthly_budget_usd: f64 = "LIVESTREAM_MONTHLY_BUDGET_USD",
    livestream_model_dir: String = "LIVESTREAM_MODEL_DIR",
    livestream_destiny_voiceprint_path: Option<String> = "LIVESTREAM_DESTINY_VOICEPRINT_PATH",
    livestream_destiny_speaker_threshold: f64 = "LIVESTREAM_DESTINY_SPEAKER_THRESHOLD",
    livestream_max_voice_targets: u32 = "LIVESTREAM_MAX_VOICE_TARGETS",
    livestream_voice_sample_seconds: u32 = "LIVESTREAM_VOICE_SAMPLE_SECONDS",
    livestream_voice_sample_interval_seconds: u32 = "LIVESTREAM_VOICE_SAMPLE_INTERVAL_SECONDS",
    livestream_summary_interval_seconds: u32 = "LIVESTREAM_SUMMARY_INTERVAL_SECONDS",
    pushover_calendar_token: Option<String> = "PUSHOVER_CALENDAR_TOKEN",
    google_generative_ai_api_key: Option<String> = "GOOGLE_GENERATIVE_AI_API_KEY",
    anthropic_api_key: Option<String> = "ANTHROPIC_API_KEY",
    openai_api_key: Option<String> = "OPENAI_API_KEY",
    tavily_api_key: Option<String> = "TAVILY_API_KEY",
    logs_path: Option<String> = "LOGS_PATH",
    icloud_username: Option<String> = "ICLOUD_USERNAME",
    icloud_reminders_enabled: Option<String> = "ICLOUD_REMINDERS_ENABLED",
    icloud_reminders_account: Option<String> = "ICLOUD_REMINDERS_ACCOUNT",
    icloud_reminders_password: Option<String> = "ICLOUD_REMINDERS_PASSWORD",
    icloud_reminders_storage_key: Option<String> = "ICLOUD_REMINDERS_STORAGE_KEY",
    icloud_reminders_public_origin: Option<String> = "ICLOUD_REMINDERS_PUBLIC_ORIGIN",
    icloud_app_password: Option<String> = "ICLOUD_APP_PASSWORD",
    icloud_calendar_name: Option<String> = "ICLOUD_CALENDAR_NAME",
    icloud_calendar_url: Option<String> = "ICLOUD_CALENDAR_URL",
    /// Raw value; use [`Config::email_self_address`] for the derived fallback.
    email_self_address: Option<String> = "EMAIL_SELF_ADDRESS",
    parcel_api_key: Option<String> = "PARCEL_API_KEY",
    karakeep_url: Option<String> = "KARAKEEP_URL",
    karakeep_api_key: Option<String> = "KARAKEEP_API_KEY",
    extraction_model: Option<String> = "EXTRACTION_MODEL",
    calendar_extraction_model: Option<String> = "CALENDAR_EXTRACTION_MODEL",
    triage_model: Option<String> = "TRIAGE_MODEL",
    tmdb_api_key: Option<String> = "TMDB_API_KEY",
    recs_shortlist_model: Option<String> = "RECS_SHORTLIST_MODEL",
    recs_selection_model: Option<String> = "RECS_SELECTION_MODEL",
    taste_reflection_model: Option<String> = "TASTE_REFLECTION_MODEL",
    taste_reflection_schedule: String = "TASTE_REFLECTION_SCHEDULE",
    recs_schedule: String = "RECS_SCHEDULE",
    recs_public_url: String = "RECS_PUBLIC_URL",
    pushover_recs_token: Option<String> = "PUSHOVER_RECS_TOKEN",
    podcast_recs_schedule: String = "PODCAST_RECS_SCHEDULE",
    podcast_taste_path: Option<String> = "PODCAST_TASTE_PATH",
    podcast_taste_reflection_model: Option<String> = "PODCAST_TASTE_REFLECTION_MODEL",
    podcast_taste_reflection_schedule: String = "PODCAST_TASTE_REFLECTION_SCHEDULE",
    pushover_podcast_token: Option<String> = "PUSHOVER_PODCAST_TOKEN",
    /// Must be a UUID when set.
    castro_access_id: Option<String> = "CASTRO_ACCESS_ID",
    castro_secret_key: Option<String> = "CASTRO_SECRET_KEY",
    podcastindex_key: Option<String> = "PODCASTINDEX_KEY",
    podcastindex_secret: Option<String> = "PODCASTINDEX_SECRET",
    podcast_voice_rotation_max: u32 = "PODCAST_VOICE_ROTATION_MAX",
    podcast_max_guest_picks: u32 = "PODCAST_MAX_GUEST_PICKS",
    plex_url: Option<String> = "PLEX_URL",
    plex_token: Option<String> = "PLEX_TOKEN",
    plex_account_id: Option<u64> = "PLEX_ACCOUNT_ID",
    radarr_url: Option<String> = "RADARR_URL",
    radarr_api_key: Option<String> = "RADARR_API_KEY",
    radarr_root_folder_path: Option<String> = "RADARR_ROOT_FOLDER_PATH",
    radarr_quality_profile_id: Option<u64> = "RADARR_QUALITY_PROFILE_ID",
    sonarr_url: Option<String> = "SONARR_URL",
    sonarr_api_key: Option<String> = "SONARR_API_KEY",
    sonarr_root_folder_path: Option<String> = "SONARR_ROOT_FOLDER_PATH",
    sonarr_quality_profile_id: Option<u64> = "SONARR_QUALITY_PROFILE_ID",
    observer_url: Option<String> = "OBSERVER_URL",
    observer_api_key: Option<String> = "OBSERVER_API_KEY",
    observer_repair_enabled: bool = "OBSERVER_REPAIR_ENABLED",
    arr_recovery_enabled: bool = "ARR_RECOVERY_ENABLED",
    nzbget_url: Option<String> = "NZBGET_URL",
    arr_recovery_local_files: bool = "ARR_RECOVERY_LOCAL_FILES",
    tz: String = "TZ",
    smtp_host: Option<String> = "SMTP_HOST",
    /// Finite number (`""` coerces to 0); default 587.
    smtp_port: f64 = "SMTP_PORT",
    smtp_user: Option<String> = "SMTP_USER",
    smtp_pass: Option<String> = "SMTP_PASS",
    /// Only `""` or [`OUTGOING_EMAIL_FROM`]; anything else fails boot.
    email_from: Option<String> = "EMAIL_FROM",
    logs_email_to: Option<String> = "LOGS_EMAIL_TO",
    whisker_credentials: Option<WhiskerCredentials> = "WHISKER_CREDENTIALS",
    /// Finite number (`""` coerces to 0); default 3000.
    frontend_port: f64 = "FRONTEND_PORT",
    omni_mcp_token: Option<String> = "OMNI_MCP_TOKEN",
    omni_device_link_token: Option<String> = "OMNI_DEVICE_LINK_TOKEN",
    omni_events_executor_auth_url: Option<String> = "OMNI_EVENTS_EXECUTOR_AUTH_URL",
    /// The MCP gateway's public `https://` origin (no path), for downstream
    /// OAuth and upstream OAuth callbacks; unset keeps the gateway LAN-only.
    omni_gateway_public_url: Option<String> = "OMNI_GATEWAY_PUBLIC_URL",
    /// HTTP(S) without credentials, query or fragment; trailing slashes trimmed.
    hister_url: String = "HISTER_URL",
    hister_access_token: Option<String> = "HISTER_ACCESS_TOKEN",
    /// `ipp://` or `ipps://`.
    printer_ipp_url: Option<String> = "PRINTER_IPP_URL",
    presspods_auth_token: Option<String> = "PRESSPODS_AUTH_TOKEN",
    presspods_public_url: Option<String> = "PRESSPODS_PUBLIC_URL",
    presspods_audio_dir: Option<String> = "PRESSPODS_AUDIO_DIR",
    presspods_metadata_model: Option<String> = "PRESSPODS_METADATA_MODEL",
    presspods_cleaning_model: Option<String> = "PRESSPODS_CLEANING_MODEL",
    presspods_tts_provider: TtsProvider = "PRESSPODS_TTS_PROVIDER",
    presspods_tts_url: Option<String> = "PRESSPODS_TTS_URL",
    presspods_tts_model: Option<String> = "PRESSPODS_TTS_MODEL",
    presspods_stt_url: Option<String> = "PRESSPODS_STT_URL",
    presspods_stt_model: Option<String> = "PRESSPODS_STT_MODEL",
    presspods_higgs_ref_audio: Option<String> = "PRESSPODS_HIGGS_REF_AUDIO",
    presspods_higgs_ref_text: Option<String> = "PRESSPODS_HIGGS_REF_TEXT",
    elevenlabs_api_key: Option<String> = "ELEVENLABS_API_KEY",
    elevenlabs_voice_male: Option<String> = "ELEVENLABS_VOICE_MALE",
    elevenlabs_voice_female: Option<String> = "ELEVENLABS_VOICE_FEMALE",
    mistral_api_key: Option<String> = "MISTRAL_API_KEY",
    jina_api_key: Option<String> = "JINA_API_KEY",
    pushover_presspods_token: Option<String> = "PUSHOVER_PRESSPODS_TOKEN",
    /// At least 24 characters when set.
    ios_control_auth_token: Option<String> = "IOS_CONTROL_AUTH_TOKEN",
    ios_control_home_url: String = "IOS_CONTROL_HOME_URL",
    ios_control_apns_team_id: Option<String> = "IOS_CONTROL_APNS_TEAM_ID",
    ios_control_apns_key_id: Option<String> = "IOS_CONTROL_APNS_KEY_ID",
    ios_control_apns_key_path: Option<String> = "IOS_CONTROL_APNS_KEY_PATH",
    ios_control_bundle_id: String = "IOS_CONTROL_BUNDLE_ID",
    /// Process-level debug switch (presence only).
    omni_debug: bool = "OMNI_DEBUG",
    /// `FFMPEG_PATH`, falling back to `ffmpeg` when unset or empty.
    ffmpeg_path: Option<String> = "FFMPEG_PATH",
    /// `YT_DLP_PATH`, falling back to `yt-dlp` when unset or empty.
    yt_dlp_path: Option<String> = "YT_DLP_PATH",
}

/// Keys always redacted in the config summary (`privateConfigKeys`).
const PRIVATE_CONFIG_KEYS: &[&str] = &[
    "PUSHOVER_USER",
    "ICLOUD_USERNAME",
    "ICLOUD_REMINDERS_ACCOUNT",
    "ICLOUD_REMINDERS_PASSWORD",
    "ICLOUD_REMINDERS_STORAGE_KEY",
    "EMAIL_SELF_ADDRESS",
    "SMTP_USER",
    "EMAIL_FROM",
    "LOGS_EMAIL_TO",
];

/// Key patterns whose values are redacted (case-insensitive).
static SENSITIVE_KEY_PATTERNS: LazyLock<Option<regex::RegexSet>> = LazyLock::new(|| {
    regex::RegexSetBuilder::new([
        r"api[_-]?key",
        r"secret",
        r"token",
        r"password",
        r"passwd",
        r"credential",
        r"private[_-]?key",
        r"auth[_-]?key",
        r"access[_-]?key",
        r"client[_-]?secret",
        r"signing[_-]?key",
        r"encryption[_-]?key",
        r"bearer",
        r"jwt",
        r"ssh[_-]?key",
        r"pgp",
        r"gpg",
        r"webhook[_-]?secret",
        r"api[_-]?secret",
        r"app[_-]?secret",
        r"hmac",
        r"salt",
        r"pin",
        r"otp",
        r"mfa",
        r"2fa",
        r"totp",
        r"recovery[_-]?code",
        r"backup[_-]?code",
    ])
    .case_insensitive(true)
    .build()
    .ok()
});

/// Whether `key` names a secret. If the fixed pattern set ever failed to compile,
/// every key is treated as sensitive.
pub fn is_sensitive_key(key: &str) -> bool {
    SENSITIVE_KEY_PATTERNS
        .as_ref()
        .is_none_or(|patterns| patterns.is_match(key))
}

/// At least 32 chars, no whitespace, 12+ distinct chars.
pub fn is_strong_mcp_token(token: &str) -> bool {
    if omni_core::js::utf16_len(token) < MIN_MCP_TOKEN_LENGTH {
        return false;
    }
    if token.chars().any(char::is_whitespace) {
        return false;
    }
    // `new Set(token)` iterates code points.
    let distinct: std::collections::BTreeSet<char> = token.chars().collect();
    distinct.len() >= MIN_DISTINCT_TOKEN_CHARS
}

/// Boot-time configuration failure.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("invalid {key}: {reason}")]
    Invalid { key: &'static str, reason: String },
    #[error("{0} must be at least 32 characters with sufficient character diversity")]
    WeakToken(&'static str),
    #[error("OMNI_DEVICE_LINK_TOKEN must differ from OMNI_MCP_TOKEN")]
    TokensEqual,
    #[error("EMAIL_FROM must be empty or {OUTGOING_EMAIL_FROM}, got {0:?}")]
    EmailFrom(String),
}

impl Config {
    /// Decodes an explicit environment without reading process globals; fails
    /// boot on the first invalid value (in declaration order), then applies the
    /// MCP and device-link token rules.
    /// Unknown variables are ignored.
    pub fn from_env(vars: &BTreeMap<String, String>) -> Result<Config, ConfigError> {
        let env = Env(vars);
        let config = Config {
            log_level: match env.get("LOG_LEVEL") {
                None => LogLevel::Info,
                Some(raw) => LogLevel::parse(raw).ok_or_else(|| {
                    invalid(
                        "LOG_LEVEL",
                        r#"expected "debug" | "info" | "warn" | "error""#,
                    )
                })?,
            },
            pushover_user: env.opt("PUSHOVER_USER"),
            pushover_token: env.opt("PUSHOVER_TOKEN"),
            dockerized: env.boolean("DOCKERIZED", false)?,
            db_name: env.string("DB_NAME", "docstore.db"),
            kick_client_id: env.opt("KICK_CLIENT_ID"),
            kick_client_secret: env.opt("KICK_CLIENT_SECRET"),
            offline_notifications: env.boolean("OFFLINE_NOTIFICATIONS", true)?,
            pushover_live_token: env.opt("PUSHOVER_LIVE_TOKEN"),
            livestream_intelligence_enabled: env
                .boolean("LIVESTREAM_INTELLIGENCE_ENABLED", false)?,
            livestream_intelligence_model: env
                .optional_literal("LIVESTREAM_INTELLIGENCE_MODEL", &["openai:gpt-6-luna"])?,
            livestream_monthly_budget_usd: {
                let key = "LIVESTREAM_MONTHLY_BUDGET_USD";
                let value = env.non_negative_with_default(key, 3.0)?;
                if value > 10.0 {
                    return Err(invalid(key, "expected a number <= 10"));
                }
                value
            },
            livestream_model_dir: env.string(
                "LIVESTREAM_MODEL_DIR",
                "/app/assets/livestream-intelligence/models",
            ),
            livestream_destiny_voiceprint_path: env.opt("LIVESTREAM_DESTINY_VOICEPRINT_PATH"),
            livestream_destiny_speaker_threshold: {
                let key = "LIVESTREAM_DESTINY_SPEAKER_THRESHOLD";
                let value = env.finite(key)?.unwrap_or(0.62);
                if !(0.0..=1.0).contains(&value) {
                    return Err(invalid(key, "expected a number between 0 and 1"));
                }
                value
            },
            livestream_max_voice_targets: env
                .positive_int_with_default("LIVESTREAM_MAX_VOICE_TARGETS", 3)?,
            livestream_voice_sample_seconds: env
                .positive_int_with_default("LIVESTREAM_VOICE_SAMPLE_SECONDS", 18)?,
            livestream_voice_sample_interval_seconds: env
                .positive_int_with_default("LIVESTREAM_VOICE_SAMPLE_INTERVAL_SECONDS", 45)?,
            livestream_summary_interval_seconds: env
                .positive_int_with_default("LIVESTREAM_SUMMARY_INTERVAL_SECONDS", 480)?,
            pushover_calendar_token: env.opt("PUSHOVER_CALENDAR_TOKEN"),
            google_generative_ai_api_key: env.opt("GOOGLE_GENERATIVE_AI_API_KEY"),
            anthropic_api_key: env.opt("ANTHROPIC_API_KEY"),
            openai_api_key: env.opt("OPENAI_API_KEY"),
            tavily_api_key: env.opt("TAVILY_API_KEY"),
            logs_path: env.opt("LOGS_PATH"),
            icloud_username: env.opt("ICLOUD_USERNAME"),
            icloud_reminders_enabled: env.opt("ICLOUD_REMINDERS_ENABLED"),
            icloud_reminders_account: env.opt("ICLOUD_REMINDERS_ACCOUNT"),
            icloud_reminders_password: env.opt("ICLOUD_REMINDERS_PASSWORD"),
            icloud_reminders_storage_key: env.opt("ICLOUD_REMINDERS_STORAGE_KEY"),
            icloud_reminders_public_origin: env.opt("ICLOUD_REMINDERS_PUBLIC_ORIGIN"),
            icloud_app_password: env.opt("ICLOUD_APP_PASSWORD"),
            icloud_calendar_name: env.opt("ICLOUD_CALENDAR_NAME"),
            icloud_calendar_url: env.opt("ICLOUD_CALENDAR_URL"),
            email_self_address: env.opt("EMAIL_SELF_ADDRESS"),
            parcel_api_key: env.opt("PARCEL_API_KEY"),
            karakeep_url: env.opt("KARAKEEP_URL"),
            karakeep_api_key: env.opt("KARAKEEP_API_KEY"),
            extraction_model: env.opt("EXTRACTION_MODEL"),
            calendar_extraction_model: env.opt("CALENDAR_EXTRACTION_MODEL"),
            triage_model: env.opt("TRIAGE_MODEL"),
            tmdb_api_key: env.opt("TMDB_API_KEY"),
            recs_shortlist_model: env.opt("RECS_SHORTLIST_MODEL"),
            recs_selection_model: env.opt("RECS_SELECTION_MODEL"),
            taste_reflection_model: env.opt("TASTE_REFLECTION_MODEL"),
            taste_reflection_schedule: env.string("TASTE_REFLECTION_SCHEDULE", "0 0 4 * * 0"),
            recs_schedule: env.string("RECS_SCHEDULE", "0 0 17 * * 1,3,5"),
            recs_public_url: env.string("RECS_PUBLIC_URL", "http://omni.boris"),
            pushover_recs_token: env.opt("PUSHOVER_RECS_TOKEN"),
            podcast_recs_schedule: env.string("PODCAST_RECS_SCHEDULE", "0 0 11 * * 1,3,5"),
            podcast_taste_path: env.opt("PODCAST_TASTE_PATH"),
            podcast_taste_reflection_model: env.opt("PODCAST_TASTE_REFLECTION_MODEL"),
            podcast_taste_reflection_schedule: env
                .string("PODCAST_TASTE_REFLECTION_SCHEDULE", "0 0 5 * * 0"),
            pushover_podcast_token: env.opt("PUSHOVER_PODCAST_TOKEN"),
            castro_access_id: match env.opt("CASTRO_ACCESS_ID") {
                Some(id) if !is_uuid(&id) => {
                    return Err(invalid("CASTRO_ACCESS_ID", "expected a UUID"));
                }
                other => other,
            },
            castro_secret_key: env.opt("CASTRO_SECRET_KEY"),
            podcastindex_key: env.opt("PODCASTINDEX_KEY"),
            podcastindex_secret: env.opt("PODCASTINDEX_SECRET"),
            podcast_voice_rotation_max: env
                .positive_int_with_default("PODCAST_VOICE_ROTATION_MAX", 12)?,
            podcast_max_guest_picks: env.positive_int_with_default("PODCAST_MAX_GUEST_PICKS", 6)?,
            plex_url: env.opt("PLEX_URL"),
            plex_token: env.opt("PLEX_TOKEN"),
            plex_account_id: env.optional_positive_int("PLEX_ACCOUNT_ID")?,
            radarr_url: env.opt("RADARR_URL"),
            radarr_api_key: env.opt("RADARR_API_KEY"),
            radarr_root_folder_path: env.opt("RADARR_ROOT_FOLDER_PATH"),
            radarr_quality_profile_id: env.optional_positive_int("RADARR_QUALITY_PROFILE_ID")?,
            sonarr_url: env.opt("SONARR_URL"),
            sonarr_api_key: env.opt("SONARR_API_KEY"),
            sonarr_root_folder_path: env.opt("SONARR_ROOT_FOLDER_PATH"),
            sonarr_quality_profile_id: env.optional_positive_int("SONARR_QUALITY_PROFILE_ID")?,
            observer_url: env.opt("OBSERVER_URL"),
            observer_api_key: env.opt("OBSERVER_API_KEY"),
            observer_repair_enabled: env.boolean("OBSERVER_REPAIR_ENABLED", true)?,
            arr_recovery_enabled: env.boolean("ARR_RECOVERY_ENABLED", true)?,
            nzbget_url: env.opt("NZBGET_URL"),
            arr_recovery_local_files: env.boolean("ARR_RECOVERY_LOCAL_FILES", false)?,
            tz: env.string("TZ", "America/Vancouver"),
            smtp_host: env.opt("SMTP_HOST"),
            smtp_port: env.finite("SMTP_PORT")?.unwrap_or(587.0),
            smtp_user: env.opt("SMTP_USER"),
            smtp_pass: env.opt("SMTP_PASS"),
            email_from: match env.opt("EMAIL_FROM") {
                Some(from) if !from.is_empty() && from != OUTGOING_EMAIL_FROM => {
                    return Err(ConfigError::EmailFrom(from));
                }
                other => other,
            },
            logs_email_to: env.opt("LOGS_EMAIL_TO"),
            whisker_credentials: env
                .opt("WHISKER_CREDENTIALS")
                .map(|raw| {
                    WhiskerCredentials::parse(&raw).ok_or_else(|| {
                        invalid(
                            "WHISKER_CREDENTIALS",
                            "WHISKER_CREDENTIALS must be email:password",
                        )
                    })
                })
                .transpose()?,
            frontend_port: env.finite("FRONTEND_PORT")?.unwrap_or(3000.0),
            omni_mcp_token: env.opt("OMNI_MCP_TOKEN"),
            omni_device_link_token: env.opt("OMNI_DEVICE_LINK_TOKEN"),
            omni_events_executor_auth_url: env.opt("OMNI_EVENTS_EXECUTOR_AUTH_URL"),
            omni_gateway_public_url: match env.opt("OMNI_GATEWAY_PUBLIC_URL") {
                None => None,
                Some(raw) => Some(public_origin("OMNI_GATEWAY_PUBLIC_URL", &raw)?),
            },
            hister_url: {
                let key = "HISTER_URL";
                let url = env.trimmed_url(key, "https://hister.syas.ca")?;
                if !is_plain_http_url(&url) {
                    return Err(invalid(
                        key,
                        "HISTER_URL must be an HTTP(S) URL without credentials, query, or fragment",
                    ));
                }
                url
            },
            hister_access_token: env.opt("HISTER_ACCESS_TOKEN"),
            printer_ipp_url: match env.opt("PRINTER_IPP_URL") {
                None => None,
                Some(raw) => {
                    let key = "PRINTER_IPP_URL";
                    let url = parse_url(key, &raw)?;
                    if !matches!(url.scheme(), "ipp" | "ipps") {
                        return Err(invalid(key, "PRINTER_IPP_URL must use ipp:// or ipps://"));
                    }
                    Some(raw)
                }
            },
            presspods_auth_token: env.opt("PRESSPODS_AUTH_TOKEN"),
            presspods_public_url: env
                .opt("PRESSPODS_PUBLIC_URL")
                .map(|v| trim_trailing_slashes(&v)),
            presspods_audio_dir: env.opt("PRESSPODS_AUDIO_DIR"),
            presspods_metadata_model: env.opt("PRESSPODS_METADATA_MODEL"),
            presspods_cleaning_model: env.opt("PRESSPODS_CLEANING_MODEL"),
            presspods_tts_provider: match env.get("PRESSPODS_TTS_PROVIDER") {
                None | Some("higgs") => TtsProvider::Higgs,
                Some("elevenlabs") => TtsProvider::Elevenlabs,
                Some(_) => {
                    return Err(invalid(
                        "PRESSPODS_TTS_PROVIDER",
                        r#"expected "higgs" | "elevenlabs""#,
                    ));
                }
            },
            presspods_tts_url: env
                .opt("PRESSPODS_TTS_URL")
                .map(|v| trim_trailing_slashes(&v)),
            presspods_tts_model: env.opt("PRESSPODS_TTS_MODEL"),
            presspods_stt_url: env
                .opt("PRESSPODS_STT_URL")
                .map(|v| trim_trailing_slashes(&v)),
            presspods_stt_model: env.opt("PRESSPODS_STT_MODEL"),
            presspods_higgs_ref_audio: env.opt("PRESSPODS_HIGGS_REF_AUDIO"),
            presspods_higgs_ref_text: env.opt("PRESSPODS_HIGGS_REF_TEXT"),
            elevenlabs_api_key: env.opt("ELEVENLABS_API_KEY"),
            elevenlabs_voice_male: env.opt("ELEVENLABS_VOICE_MALE"),
            elevenlabs_voice_female: env.opt("ELEVENLABS_VOICE_FEMALE"),
            mistral_api_key: env.opt("MISTRAL_API_KEY"),
            jina_api_key: env.opt("JINA_API_KEY"),
            pushover_presspods_token: env.opt("PUSHOVER_PRESSPODS_TOKEN"),
            ios_control_auth_token: match env.opt("IOS_CONTROL_AUTH_TOKEN") {
                Some(token) if omni_core::js::utf16_len(&token) < 24 => {
                    return Err(invalid(
                        "IOS_CONTROL_AUTH_TOKEN",
                        "expected a value with a length of at least 24",
                    ));
                }
                other => other,
            },
            ios_control_home_url: env.trimmed_url("IOS_CONTROL_HOME_URL", "http://omni.boris")?,
            ios_control_apns_team_id: env.opt("IOS_CONTROL_APNS_TEAM_ID"),
            ios_control_apns_key_id: env.opt("IOS_CONTROL_APNS_KEY_ID"),
            ios_control_apns_key_path: env.opt("IOS_CONTROL_APNS_KEY_PATH"),
            ios_control_bundle_id: env.string("IOS_CONTROL_BUNDLE_ID", "com.micthiesen.OmniLive"),
            omni_debug: env.get("OMNI_DEBUG").is_some(),
            ffmpeg_path: env.opt("FFMPEG_PATH"),
            yt_dlp_path: env.opt("YT_DLP_PATH"),
        };
        validate_mcp_token(config.omni_mcp_token.as_deref(), config.dockerized)?;
        validate_device_link_token(
            config.omni_device_link_token.as_deref(),
            config.omni_mcp_token.as_deref(),
        )?;
        Ok(config)
    }

    /// `FFMPEG_PATH || "ffmpeg"`.
    pub fn ffmpeg_bin(&self) -> &str {
        self.ffmpeg_path
            .as_deref()
            .filter(|p| !p.is_empty())
            .unwrap_or("ffmpeg")
    }

    /// `YT_DLP_PATH || "yt-dlp"`.
    pub fn yt_dlp_bin(&self) -> &str {
        self.yt_dlp_path
            .as_deref()
            .filter(|p| !p.is_empty())
            .unwrap_or("yt-dlp")
    }

    /// [`Config::from_env`] over `std::env::vars()`.
    pub fn from_process_env() -> Result<Config, ConfigError> {
        let vars: BTreeMap<String, String> = std::env::vars().collect();
        Config::from_env(&vars)
    }

    /// `(KEY, value)` for every variable with private and sensitive keys as `***`.
    pub fn redacted_summary(&self) -> Vec<(&'static str, String)> {
        self.entries()
            .into_iter()
            .map(|(key, value)| {
                if PRIVATE_CONFIG_KEYS.contains(&key) || is_sensitive_key(key) {
                    (key, "***".to_owned())
                } else {
                    (key, value)
                }
            })
            .collect()
    }

    /// `DOCKERIZED ? /data/<DB_NAME> : <DB_NAME>` (string concatenation).
    pub fn db_path(&self) -> PathBuf {
        if self.dockerized {
            PathBuf::from(format!("/data/{}", self.db_name))
        } else {
            PathBuf::from(&self.db_name)
        }
    }

    /// Token for a channel with the derived `?? PUSHOVER_TOKEN` fallback.
    pub fn pushover_token(&self, ch: PushoverChannel) -> Option<&str> {
        let specific = match ch {
            PushoverChannel::General => None,
            PushoverChannel::Live => self.pushover_live_token.as_deref(),
            PushoverChannel::Calendar => self.pushover_calendar_token.as_deref(),
            PushoverChannel::Recs => self.pushover_recs_token.as_deref(),
            PushoverChannel::Podcast => self.pushover_podcast_token.as_deref(),
            PushoverChannel::PressPods => self.pushover_presspods_token.as_deref(),
        };
        specific.or(self.pushover_token.as_deref())
    }

    /// `EMAIL_SELF_ADDRESS ?? ICLOUD_USERNAME`.
    pub fn email_self_address(&self) -> Option<&str> {
        self.email_self_address
            .as_deref()
            .or(self.icloud_username.as_deref())
    }

    /// The model id for a role: env override, else the code default.
    pub fn model(&self, role: ModelRole) -> &str {
        let configured = match role {
            ModelRole::Extraction => self.extraction_model.as_deref(),
            ModelRole::CalendarExtraction => self.calendar_extraction_model.as_deref(),
            ModelRole::Triage => self.triage_model.as_deref(),
            ModelRole::RecsShortlist => self.recs_shortlist_model.as_deref(),
            ModelRole::RecsSelection => self.recs_selection_model.as_deref(),
            ModelRole::TasteReflection => self.taste_reflection_model.as_deref(),
            ModelRole::PodcastTasteReflection => self.podcast_taste_reflection_model.as_deref(),
            ModelRole::LivestreamIntelligence => self.livestream_intelligence_model.as_deref(),
            ModelRole::PressPodsMetadata => self.presspods_metadata_model.as_deref(),
            ModelRole::PressPodsCleaning => self.presspods_cleaning_model.as_deref(),
            ModelRole::ObserverRepair | ModelRole::ArrRecovery => None,
        };
        configured.unwrap_or(role.default_model())
    }
}

fn invalid(key: &'static str, reason: impl Into<String>) -> ConfigError {
    ConfigError::Invalid {
        key,
        reason: reason.into(),
    }
}

/// The raw environment with one decoder per field kind.
struct Env<'a>(&'a BTreeMap<String, String>);

impl Env<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    /// Present (even empty) or absent.
    fn opt(&self, key: &str) -> Option<String> {
        self.get(key).map(str::to_owned)
    }

    /// The default applies only when absent.
    fn string(&self, key: &str, default: &str) -> String {
        self.get(key).unwrap_or(default).to_owned()
    }

    /// `"true"` / `"false"` in any case.
    fn boolean(&self, key: &'static str, default: bool) -> Result<bool, ConfigError> {
        match self.get(key).map(str::to_lowercase).as_deref() {
            None => Ok(default),
            Some("true") => Ok(true),
            Some("false") => Ok(false),
            Some(_) => Err(invalid(key, r#"Expected "true" or "false""#)),
        }
    }

    /// A finite number parsed like JS `Number(s)`, so `""` is 0; `None` when absent.
    fn finite(&self, key: &'static str) -> Result<Option<f64>, ConfigError> {
        self.get(key)
            .map(|raw| {
                let value = omni_core::js::string_to_number(raw);
                if value.is_finite() {
                    Ok(value)
                } else {
                    Err(invalid(key, "expected a finite number"))
                }
            })
            .transpose()
    }

    /// A positive integer (a finite integer > 0).
    fn positive_int(key: &'static str, raw: &str) -> Result<u64, ConfigError> {
        let value = omni_core::js::string_to_number(raw);
        if !value.is_finite()
            || value.fract() != 0.0
            || value <= 0.0
            || value > 9_007_199_254_740_991.0
        {
            return Err(invalid(key, "expected a positive integer"));
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Ok(value as u64)
    }

    /// `positiveIntegerWithDefault(d)`: absent or `""` is `d`.
    fn positive_int_with_default(
        &self,
        key: &'static str,
        default: u32,
    ) -> Result<u32, ConfigError> {
        match self.get(key) {
            None | Some("") => Ok(default),
            Some(raw) => u32::try_from(Self::positive_int(key, raw)?)
                .map_err(|_| invalid(key, "expected a positive integer below 2^32")),
        }
    }

    /// Absent or `""` is `None`.
    fn optional_positive_int(&self, key: &'static str) -> Result<Option<u64>, ConfigError> {
        match self.get(key) {
            None | Some("") => Ok(None),
            Some(raw) => Self::positive_int(key, raw).map(Some),
        }
    }

    /// `nonNegativeNumberWithDefault(d)`: absent or `""` is `d`.
    fn non_negative_with_default(
        &self,
        key: &'static str,
        default: f64,
    ) -> Result<f64, ConfigError> {
        match self.get(key) {
            None | Some("") => Ok(default),
            Some(_) => match self.finite(key)? {
                Some(value) if value >= 0.0 => Ok(value),
                _ => Err(invalid(key, "expected a number >= 0")),
            },
        }
    }

    fn optional_literal(
        &self,
        key: &'static str,
        allowed: &[&str],
    ) -> Result<Option<String>, ConfigError> {
        match self.get(key) {
            None => Ok(None),
            Some(raw) if allowed.contains(&raw) => Ok(Some(raw.to_owned())),
            Some(_) => Err(invalid(key, format!("expected one of {allowed:?}"))),
        }
    }

    /// A valid URL, then trailing slashes trimmed.
    fn trimmed_url(&self, key: &'static str, default: &str) -> Result<String, ConfigError> {
        match self.get(key) {
            None => Ok(default.to_owned()),
            Some(raw) => {
                parse_url(key, raw)?;
                Ok(trim_trailing_slashes(raw))
            }
        }
    }
}

/// An `https://host[:port]` origin without credentials, path, query or fragment.
fn public_origin(key: &'static str, raw: &str) -> Result<String, ConfigError> {
    let url = parse_url(key, raw.trim())?;
    let plain = url.scheme() == "https"
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && matches!(url.path(), "" | "/")
        && url.query().is_none()
        && url.fragment().is_none();
    if !plain {
        return Err(invalid(
            key,
            "Expected an https:// origin without credentials, path, query or fragment",
        ));
    }
    Ok(url.origin().ascii_serialization())
}

/// `new URL(value)` (WHATWG parsing).
fn parse_url(key: &'static str, raw: &str) -> Result<url::Url, ConfigError> {
    url::Url::parse(raw).map_err(|_| invalid(key, "Expected a valid URL"))
}

/// `value.replace(/\/+$/, "")`.
fn trim_trailing_slashes(value: &str) -> String {
    value.trim_end_matches('/').to_owned()
}

/// `HISTER_URL`: http(s), no credentials, no non-empty query or fragment.
fn is_plain_http_url(value: &str) -> bool {
    let Ok(url) = url::Url::parse(value) else {
        return false;
    };
    matches!(url.scheme(), "http" | "https")
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none_or(str::is_empty)
        && url.fragment().is_none_or(str::is_empty)
}

/// A UUID string: versions 1-8, the nil UUID and the max UUID.
fn is_uuid(value: &str) -> bool {
    static UUID: LazyLock<Option<regex::Regex>> = LazyLock::new(|| {
        regex::Regex::new(
            "^([0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[1-8][0-9a-fA-F]{3}-[89abAB][0-9a-fA-F]{3}-[0-9a-fA-F]{12}\
             |00000000-0000-0000-0000-000000000000\
             |[fF]{8}-[fF]{4}-[fF]{4}-[fF]{4}-[fF]{12})$",
        )
        .ok()
    });
    UUID.as_ref().is_some_and(|re| re.is_match(value))
}

/// Required and strong in production; in
/// development an absent token disables MCP, but a present one must be strong.
fn validate_mcp_token(token: Option<&str>, production: bool) -> Result<(), ConfigError> {
    match token {
        None if !production => Ok(()),
        None | Some("") => Err(invalid(
            "OMNI_MCP_TOKEN",
            "OMNI_MCP_TOKEN is required in production",
        )),
        Some(token) if !is_strong_mcp_token(token) => Err(ConfigError::WeakToken("OMNI_MCP_TOKEN")),
        Some(_) => Ok(()),
    }
}

/// Optional, strong, distinct from the MCP token.
fn validate_device_link_token(token: Option<&str>, mcp: Option<&str>) -> Result<(), ConfigError> {
    match token {
        None | Some("") => Ok(()),
        Some(token) if !is_strong_mcp_token(token) => {
            Err(ConfigError::WeakToken("OMNI_DEVICE_LINK_TOKEN"))
        }
        Some(token) if Some(token) == mcp => Err(ConfigError::TokensEqual),
        Some(_) => Ok(()),
    }
}

impl WhiskerCredentials {
    /// `email:password`, split at the first `:`; both sides non-empty.
    fn parse(raw: &str) -> Option<Self> {
        let (email, password) = raw.split_once(':')?;
        if email.is_empty() || password.is_empty() {
            return None;
        }
        Some(Self {
            email: email.to_owned(),
            password: password.to_owned(),
        })
    }
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut map = f.debug_map();
        for (key, value) in self.redacted_summary() {
            map.entry(&key, &value);
        }
        map.finish()
    }
}

const LEGACY_CHANNEL_ENV_VARS: &[&str] = &[
    "YT_CHANNEL_NAMES",
    "TWITCH_CHANNEL_NAMES",
    "KICK_CHANNEL_NAMES",
    "CHANNELS_CONFIG_PATH",
];
const LEGACY_EMAIL_ENV_VARS: &[&str] = &[
    "EMAIL_TRANSPORT",
    "CALDAV_PROVIDER",
    "FASTMAIL_API_TOKEN",
    "FASTMAIL_APP_PASSWORD",
    "FASTMAIL_USERNAME",
    "FASTMAIL_CALENDAR_ID",
];
const LEGACY_FEATURE_ENV_VARS: &[&str] = &[
    "BRIEFINGS_PATH",
    "BRIEFING_MODEL",
    "PUSHOVER_BRIEFING_TOKEN",
    "WORKSPACE_MODEL",
    "WORKSPACE_SCHEDULE",
    "WORKSPACES_PUBLIC_URL",
    "PUSHOVER_WORKSPACE_TOKEN",
];

/// Boot warnings for variables that are set but no longer read.
pub fn legacy_warnings(vars: &BTreeMap<String, String>) -> Vec<String> {
    let is_set = |key: &str| vars.get(key).is_some_and(|value| !value.is_empty());
    let channel = LEGACY_CHANNEL_ENV_VARS
        .iter()
        .copied()
        .filter(|key| is_set(key))
        .map(|key| format!("{key} is no longer read, streamers are configured in Omni"));
    let email = LEGACY_EMAIL_ENV_VARS
        .iter()
        .copied()
        .filter(|key| is_set(key))
        .map(|key| format!("{key} is no longer read, email and calendar use iCloud credentials"));
    let removed = LEGACY_FEATURE_ENV_VARS
        .iter()
        .copied()
        .filter(|key| is_set(key))
        .map(|key| format!("{key} is no longer read, briefings and workspaces were removed"));
    channel.chain(email).chain(removed).collect()
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod golden_spec;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensitive_key_patterns() {
        assert!(is_sensitive_key("OPENAI_API_KEY"));
        assert!(is_sensitive_key("PUSHOVER_LIVE_TOKEN"));
        assert!(is_sensitive_key("WHISKER_CREDENTIALS"));
        assert!(!is_sensitive_key("PRINTER_IPP_URL"));
        assert!(!is_sensitive_key("TZ"));
    }

    #[test]
    fn gateway_public_url_is_an_https_origin() {
        assert_eq!(
            public_origin("K", "https://gateway.syas.ca/").unwrap(),
            "https://gateway.syas.ca"
        );
        assert_eq!(
            public_origin("K", "https://gw.example:8443").unwrap(),
            "https://gw.example:8443"
        );
        for bad in [
            "http://gateway.syas.ca",
            "https://gateway.syas.ca/gateway/mcp",
            "https://user:pw@gateway.syas.ca",
            "https://gateway.syas.ca?x=1",
            "not a url",
        ] {
            assert!(public_origin("K", bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn strong_token_rules() {
        assert!(!is_strong_mcp_token("short"));
        assert!(!is_strong_mcp_token(&"a".repeat(40)));
        assert!(is_strong_mcp_token("abcdefghijklmnopqrstuvwxyz0123456789"));
        assert!(!is_strong_mcp_token(
            "abcdefghijklmnop qrstuvwxyz0123456789"
        ));
    }

    #[test]
    fn legacy_warnings_list_set_keys() {
        let mut vars = BTreeMap::new();
        vars.insert("YT_CHANNEL_NAMES".to_owned(), "x".to_owned());
        vars.insert("CALDAV_PROVIDER".to_owned(), "fastmail".to_owned());
        vars.insert("KICK_CHANNEL_NAMES".to_owned(), String::new());
        vars.insert("BRIEFINGS_PATH".to_owned(), "/briefings".to_owned());
        assert_eq!(
            legacy_warnings(&vars),
            vec![
                "YT_CHANNEL_NAMES is no longer read, streamers are configured in Omni",
                "CALDAV_PROVIDER is no longer read, email and calendar use iCloud credentials",
                "BRIEFINGS_PATH is no longer read, briefings and workspaces were removed",
            ]
        );
    }

    #[test]
    fn model_defaults() {
        assert_eq!(ModelRole::RecsSelection.default_model(), "openai:gpt-6-sol");
        assert_eq!(ModelRole::Triage.default_model(), "openai:gpt-6-luna");
        assert_eq!(ModelRole::ArrRecovery.env_key(), None);
    }
}
