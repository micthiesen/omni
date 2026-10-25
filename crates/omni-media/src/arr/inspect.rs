//! Read-only views shared by Radarr and Sonarr, whose APIs agree on these
//! resources: quality profiles, service status and the download queue.
//!
//! Nothing here writes. Download-client and indexer settings carry
//! credentials, so only named, non-secret fields are ever read out.

use omni_http::Method;
use serde::{Deserialize, Serialize};

use super::{ArrConfig, ArrHttp, HttpResult, Limits};

/// Which service a shared view reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ArrService {
    Radarr,
    Sonarr,
}

/// Why a read produced nothing.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum InspectError {
    #[error("{0} is not configured")]
    NotConfigured(&'static str),
    #[error("{0} is unavailable")]
    Unavailable(&'static str),
    #[error("{0}'s download queue holds more than 500 downloads; it cannot be read whole")]
    QueueTruncated(&'static str),
    #[error("{service} answered HTTP {status_code}")]
    Http {
        service: &'static str,
        status_code: u16,
    },
}

impl ArrService {
    pub fn name(self) -> &'static str {
        match self {
            Self::Radarr => "Radarr",
            Self::Sonarr => "Sonarr",
        }
    }
}

/// `step` for the shared reads.
pub(crate) fn read<T>(service: ArrService, result: HttpResult<T>) -> Result<T, InspectError> {
    match result {
        HttpResult::Ok(value) => Ok(value),
        HttpResult::HttpError { status_code } => Err(InspectError::Http {
            service: service.name(),
            status_code,
        }),
        HttpResult::Unavailable => Err(InspectError::Unavailable(service.name())),
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QualityResource {
    #[serde(default)]
    pub id: Option<i64>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub resolution: Option<i64>,
    #[serde(default)]
    pub source: Option<String>,
}

/// `{quality, revision}` as Radarr and Sonarr nest it.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QualityModel {
    #[serde(default)]
    pub quality: Option<QualityResource>,
}

impl QualityModel {
    pub fn name(&self) -> Option<String> {
        self.quality.as_ref().and_then(|q| q.name.clone())
    }

    pub fn resolution(&self) -> Option<i64> {
        self.quality.as_ref().and_then(|q| q.resolution)
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NamedResource {
    #[serde(default)]
    pub id: Option<i64>,
    #[serde(default)]
    pub name: Option<String>,
}

/// The names of a list of custom formats or languages.
pub fn names(items: &[NamedResource]) -> Vec<String> {
    items.iter().filter_map(|i| i.name.clone()).collect()
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProfileItem {
    /// Set on a quality group.
    #[serde(default)]
    id: Option<i64>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    quality: Option<QualityResource>,
    #[serde(default)]
    items: Vec<ProfileItem>,
    #[serde(default)]
    allowed: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FormatItem {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    score: i64,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QualityProfileResource {
    id: i64,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    upgrade_allowed: Option<bool>,
    #[serde(default)]
    cutoff: Option<i64>,
    #[serde(default)]
    items: Vec<ProfileItem>,
    #[serde(default)]
    min_format_score: Option<i64>,
    #[serde(default)]
    cutoff_format_score: Option<i64>,
    #[serde(default)]
    min_upgrade_format_score: Option<i64>,
    #[serde(default)]
    format_items: Vec<FormatItem>,
    /// Radarr only.
    #[serde(default)]
    language: Option<NamedResource>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FormatScore {
    pub name: String,
    pub score: i64,
}

/// A quality profile as the tools present it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileSummary {
    pub id: i64,
    pub name: String,
    pub upgrade_allowed: bool,
    /// The quality (or quality group) at which upgrades by quality stop.
    pub cutoff: Option<String>,
    /// Allowed qualities, least to most preferred.
    pub allowed_qualities: Vec<String>,
    pub max_resolution: Option<i64>,
    pub min_format_score: Option<i64>,
    pub cutoff_format_score: Option<i64>,
    pub min_upgrade_format_score: Option<i64>,
    /// Custom formats with a nonzero score.
    pub custom_format_scores: Vec<FormatScore>,
    pub language: Option<String>,
    /// The profile Omni uses when it adds a title.
    pub is_default: bool,
}

impl QualityProfileResource {
    pub fn id(&self) -> i64 {
        self.id
    }

    pub fn name(&self) -> String {
        self.name.clone().unwrap_or_default()
    }

    pub fn upgrade_allowed(&self) -> bool {
        self.upgrade_allowed == Some(true)
    }

    pub fn summary(&self, default_id: Option<u64>) -> ProfileSummary {
        let mut allowed = Vec::new();
        let mut max_resolution = None;
        for item in &self.items {
            let members: Vec<&QualityResource> = if item.items.is_empty() {
                item.quality.iter().collect()
            } else {
                item.items
                    .iter()
                    .filter_map(|i| i.quality.as_ref())
                    .collect()
            };
            let item_allowed = item.allowed || item.items.iter().any(|i| i.allowed);
            if !item_allowed {
                continue;
            }
            for quality in members {
                if let Some(name) = &quality.name {
                    allowed.push(name.clone());
                }
                max_resolution = max_resolution.max(quality.resolution);
            }
        }
        let cutoff = self.cutoff.and_then(|cutoff| {
            self.items
                .iter()
                .find_map(|item| match (item.id, &item.quality) {
                    (Some(group), _) if group == cutoff => item.name.clone(),
                    (_, Some(quality)) if quality.id == Some(cutoff) => quality.name.clone(),
                    _ => None,
                })
        });
        ProfileSummary {
            id: self.id,
            name: self.name(),
            upgrade_allowed: self.upgrade_allowed(),
            cutoff,
            allowed_qualities: allowed,
            max_resolution,
            min_format_score: self.min_format_score,
            cutoff_format_score: self.cutoff_format_score,
            min_upgrade_format_score: self.min_upgrade_format_score,
            custom_format_scores: self
                .format_items
                .iter()
                .filter(|f| f.score != 0)
                .filter_map(|f| {
                    Some(FormatScore {
                        name: f.name.clone()?,
                        score: f.score,
                    })
                })
                .collect(),
            language: self.language.as_ref().and_then(|l| l.name.clone()),
            is_default: default_id.is_some_and(|d| u64::try_from(self.id).ok() == Some(d)),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SystemStatusResource {
    #[serde(default)]
    version: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthItem {
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RootFolderResource {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    accessible: Option<bool>,
    #[serde(default)]
    free_space: Option<i64>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MediaManagementResource {
    #[serde(default)]
    recycle_bin: Option<String>,
    #[serde(default)]
    copy_using_hardlinks: Option<bool>,
    #[serde(default)]
    download_propers_and_repacks: Option<String>,
    #[serde(default)]
    minimum_free_space_when_importing: Option<i64>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DownloadClientResource {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    implementation: Option<String>,
    #[serde(default)]
    protocol: Option<String>,
    #[serde(default)]
    enable: Option<bool>,
    #[serde(default)]
    remove_completed_downloads: Option<bool>,
    #[serde(default)]
    remove_failed_downloads: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RootFolder {
    pub path: String,
    pub accessible: bool,
    pub free_space: Option<i64>,
}

/// The settings that decide what an upgrade import does with the old file.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportSettings {
    /// `null`: a replaced file is deleted permanently, not recycled.
    pub recycle_bin: Option<String>,
    pub copy_using_hardlinks: Option<bool>,
    pub download_propers_and_repacks: Option<String>,
    pub minimum_free_space_when_importing_mb: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadClient {
    pub name: String,
    pub implementation: Option<String>,
    pub protocol: Option<String>,
    pub enabled: bool,
    pub remove_completed_downloads: Option<bool>,
    pub remove_failed_downloads: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceStatus {
    pub service: ArrService,
    pub version: Option<String>,
    pub health: Vec<HealthItem>,
    pub root_folders: Vec<RootFolder>,
    pub import_settings: ImportSettings,
    pub download_clients: Vec<DownloadClient>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StatusMessage {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    messages: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueueTitle {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    tmdb_id: Option<i64>,
    #[serde(default)]
    tvdb_id: Option<i64>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueueEpisodeRef {
    #[serde(default)]
    season_number: Option<i64>,
    #[serde(default)]
    episode_number: Option<i64>,
}

/// One download in a Radarr or Sonarr queue.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueRecord {
    #[serde(default)]
    id: Option<i64>,
    #[serde(default)]
    pub movie_id: Option<i64>,
    #[serde(default)]
    series_id: Option<i64>,
    #[serde(default)]
    season_number: Option<i64>,
    #[serde(default)]
    episode: Option<QueueEpisodeRef>,
    #[serde(default)]
    movie: Option<QueueTitle>,
    #[serde(default)]
    series: Option<QueueTitle>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    tracked_download_status: Option<String>,
    #[serde(default)]
    tracked_download_state: Option<String>,
    #[serde(default)]
    status_messages: Vec<StatusMessage>,
    #[serde(default)]
    error_message: Option<String>,
    #[serde(default)]
    quality: Option<QualityModel>,
    #[serde(default)]
    custom_format_score: Option<i64>,
    #[serde(default)]
    size: Option<f64>,
    #[serde(default)]
    sizeleft: Option<f64>,
    #[serde(default)]
    timeleft: Option<String>,
    #[serde(default)]
    estimated_completion_time: Option<String>,
    #[serde(default)]
    added: Option<String>,
    #[serde(default)]
    protocol: Option<String>,
    #[serde(default)]
    download_client: Option<String>,
    #[serde(default)]
    indexer: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct QueuePage {
    #[serde(default)]
    total_records: Option<i64>,
    #[serde(default)]
    pub records: Vec<QueueRecord>,
}

/// A download as the tools present it.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Download {
    pub queue_id: Option<i64>,
    pub title: Option<String>,
    /// Radarr's movie id or Sonarr's series id.
    pub item_id: Option<i64>,
    pub item_title: Option<String>,
    pub tmdb_id: Option<i64>,
    pub tvdb_id: Option<i64>,
    pub season_number: Option<i64>,
    pub episode_number: Option<i64>,
    pub status: Option<String>,
    /// `ok`, `warning` or `error`.
    pub tracked_download_status: Option<String>,
    /// For example `downloading`, `importPending`, `importBlocked`, `failedPending`.
    pub tracked_download_state: Option<String>,
    /// Why the download is stuck or was rejected, as the service reports it.
    pub messages: Vec<String>,
    pub quality: Option<String>,
    pub custom_format_score: Option<i64>,
    pub size_bytes: Option<f64>,
    pub size_left_bytes: Option<f64>,
    pub time_left: Option<String>,
    pub estimated_completion_time: Option<String>,
    pub added: Option<String>,
    pub protocol: Option<String>,
    pub download_client: Option<String>,
    pub indexer: Option<String>,
}

impl From<&QueueRecord> for Download {
    fn from(r: &QueueRecord) -> Self {
        let titled = r.movie.as_ref().or(r.series.as_ref());
        let mut messages: Vec<String> = r
            .status_messages
            .iter()
            .flat_map(|m| {
                let title = m.title.clone().unwrap_or_default();
                if m.messages.is_empty() {
                    vec![title]
                } else {
                    m.messages
                        .iter()
                        .map(|msg| {
                            if title.is_empty() {
                                msg.clone()
                            } else {
                                format!("{title}: {msg}")
                            }
                        })
                        .collect()
                }
            })
            .filter(|m| !m.is_empty())
            .collect();
        if let Some(error) = r.error_message.clone().filter(|e| !e.is_empty()) {
            messages.push(error);
        }
        let messages = messages.iter().map(|m| redact(m)).collect();
        Self {
            queue_id: r.id,
            title: r.title.as_deref().map(redact),
            item_id: r.movie_id.or(r.series_id),
            item_title: titled.and_then(|t| t.title.clone()),
            tmdb_id: r.movie.as_ref().and_then(|m| m.tmdb_id),
            tvdb_id: r.series.as_ref().and_then(|s| s.tvdb_id),
            season_number: r
                .episode
                .as_ref()
                .and_then(|e| e.season_number)
                .or(r.season_number),
            episode_number: r.episode.as_ref().and_then(|e| e.episode_number),
            status: r.status.clone(),
            tracked_download_status: r.tracked_download_status.clone(),
            tracked_download_state: r.tracked_download_state.clone(),
            messages,
            quality: r.quality.as_ref().and_then(QualityModel::name),
            custom_format_score: r.custom_format_score,
            size_bytes: r.size,
            size_left_bytes: r.sizeleft,
            time_left: r.timeleft.clone(),
            estimated_completion_time: r.estimated_completion_time.clone(),
            added: r.added.clone(),
            protocol: r.protocol.clone(),
            download_client: r.download_client.clone(),
            indexer: r.indexer.clone(),
        }
    }
}

/// The queue as read: `truncated` when the service holds more than one page.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Queue {
    pub downloads: Vec<Download>,
    pub truncated: bool,
}

/// The most queue records one read takes.
const QUEUE_PAGE: usize = 500;

/// Shared reads over both configured services.
#[derive(Clone)]
pub struct ArrInspect {
    http: ArrHttp,
    radarr: ArrConfig,
    sonarr: ArrConfig,
}

impl ArrInspect {
    pub fn new(http: ArrHttp, radarr: ArrConfig, sonarr: ArrConfig) -> Self {
        Self {
            http,
            radarr,
            sonarr,
        }
    }

    fn config(&self, service: ArrService) -> &ArrConfig {
        match service {
            ArrService::Radarr => &self.radarr,
            ArrService::Sonarr => &self.sonarr,
        }
    }

    fn connection(&self, service: ArrService) -> Result<(&str, &str), InspectError> {
        self.config(service)
            .connection()
            .ok_or(InspectError::NotConfigured(service.name()))
    }

    pub async fn profiles(&self, service: ArrService) -> Result<Vec<ProfileSummary>, InspectError> {
        let connection = self.connection(service)?;
        let default_id = self.config(service).quality_profile_id;
        Ok(fetch_profiles(&self.http, service, connection)
            .await?
            .iter()
            .map(|p| p.summary(default_id))
            .collect())
    }

    pub async fn status(&self, service: ArrService) -> Result<ServiceStatus, InspectError> {
        let connection = self.connection(service)?;
        let http = &self.http;
        let (status, health, roots, management, clients) = futures::join!(
            http.request_json::<SystemStatusResource>(connection, "system/status", None),
            http.request_json::<Vec<HealthItem>>(connection, "health", None),
            http.request_json::<Vec<RootFolderResource>>(connection, "rootfolder", None),
            http.request_json::<MediaManagementResource>(
                connection,
                "config/mediamanagement",
                None
            ),
            http.request_json::<Vec<DownloadClientResource>>(connection, "downloadclient", None),
        );
        let management = read(service, management)?;
        Ok(ServiceStatus {
            service,
            version: read(service, status)?.version,
            health: read(service, health)?
                .into_iter()
                .map(|h| HealthItem {
                    message: h.message.as_deref().map(redact),
                    ..h
                })
                .collect(),
            root_folders: read(service, roots)?
                .into_iter()
                .map(|r| RootFolder {
                    path: r.path.unwrap_or_default(),
                    accessible: r.accessible == Some(true),
                    free_space: r.free_space,
                })
                .collect(),
            import_settings: ImportSettings {
                recycle_bin: management.recycle_bin.filter(|p| !p.is_empty()),
                copy_using_hardlinks: management.copy_using_hardlinks,
                download_propers_and_repacks: management.download_propers_and_repacks,
                minimum_free_space_when_importing_mb: management.minimum_free_space_when_importing,
            },
            download_clients: read(service, clients)?
                .into_iter()
                .map(|c| DownloadClient {
                    name: c.name.unwrap_or_default(),
                    implementation: c.implementation,
                    protocol: c.protocol,
                    enabled: c.enable == Some(true),
                    remove_completed_downloads: c.remove_completed_downloads,
                    remove_failed_downloads: c.remove_failed_downloads,
                })
                .collect(),
        })
    }

    /// The whole queue (up to [`QUEUE_PAGE`] downloads), unmatched downloads included.
    pub async fn queue(&self, service: ArrService) -> Result<Queue, InspectError> {
        let connection = self.connection(service)?;
        let path = match service {
            ArrService::Radarr => format!(
                "queue?page=1&pageSize={QUEUE_PAGE}&includeMovie=true&includeUnknownMovieItems=true"
            ),
            ArrService::Sonarr => format!(
                "queue?page=1&pageSize={QUEUE_PAGE}&includeSeries=true&includeEpisode=true&includeUnknownSeriesItems=true"
            ),
        };
        let page = read(
            service,
            self.http
                .send_limited::<QueuePage>(connection, Method::GET, &path, None, Limits::LIBRARY)
                .await,
        )?;
        Ok(Queue {
            truncated: page
                .total_records
                .is_some_and(|t| usize::try_from(t).unwrap_or(usize::MAX) > page.records.len()),
            downloads: page.records.iter().map(Download::from).collect(),
        })
    }
}

/// Every configured quality profile.
pub(crate) async fn fetch_profiles(
    http: &ArrHttp,
    service: ArrService,
    connection: (&str, &str),
) -> Result<Vec<QualityProfileResource>, InspectError> {
    read(
        service,
        http.request_json::<Vec<QualityProfileResource>>(connection, "qualityprofile", None)
            .await,
    )
}

/// The whole queue, without the per-item title (for joining by item id).
/// A queue longer than one page fails: a download past it would be missed,
/// and the guards that read this must never decide from a partial queue.
pub(crate) async fn fetch_queue(
    http: &ArrHttp,
    service: ArrService,
    connection: (&str, &str),
) -> Result<Vec<QueueRecord>, InspectError> {
    let path = format!("queue?page=1&pageSize={QUEUE_PAGE}&includeUnknownMovieItems=true");
    let page = read(
        service,
        http.send_limited::<QueuePage>(connection, Method::GET, &path, None, Limits::LIBRARY)
            .await,
    )?;
    if page
        .total_records
        .is_some_and(|t| usize::try_from(t).unwrap_or(usize::MAX) > page.records.len())
    {
        return Err(InspectError::QueueTruncated(service.name()));
    }
    Ok(page.records)
}

/// Masks credentials in free text from Radarr or Sonarr (download-client and
/// indexer errors can quote request URLs): the values of key-like query
/// parameters and URL userinfo.
pub fn redact(text: &str) -> String {
    static QUERY_SECRET: std::sync::LazyLock<Option<regex::Regex>> = std::sync::LazyLock::new(
        || {
            regex::Regex::new(
                r"(?i)\b(apikey|api_key|api-key|passkey|rsskey|authkey|token|access_token|password|pass|secret|key|sig|signature)=[^&\s'\x22<>]+",
            )
            .ok()
        },
    );
    static USERINFO: std::sync::LazyLock<Option<regex::Regex>> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?i)\b([a-z][a-z0-9+.-]*://)[^/\s@'\x22]+@").ok()
    });
    let mut out = text.to_owned();
    if let Some(re) = QUERY_SECRET.as_ref() {
        out = re.replace_all(&out, "$1=<redacted>").into_owned();
    }
    if let Some(re) = USERINFO.as_ref() {
        out = re.replace_all(&out, "$1<redacted>@").into_owned();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn profile() -> QualityProfileResource {
        serde_json::from_value(json!({
            "id": 9, "name": "4K Quality", "upgradeAllowed": true, "cutoff": 1001,
            "minFormatScore": 0, "cutoffFormatScore": 10000, "minUpgradeFormatScore": 1,
            "language": {"id": -2, "name": "Original"},
            "items": [
                {"quality": {"id": 1, "name": "SDTV", "resolution": 480}, "items": [], "allowed": false},
                {"quality": {"id": 7, "name": "Bluray-1080p", "resolution": 1080}, "items": [], "allowed": true},
                {"id": 1001, "name": "UHD", "allowed": true, "items": [
                    {"quality": {"id": 19, "name": "Bluray-2160p", "resolution": 2160}, "items": [], "allowed": true},
                    {"quality": {"id": 31, "name": "Remux-2160p", "resolution": 2160}, "items": [], "allowed": true}
                ]}
            ],
            "formatItems": [
                {"format": 3, "name": "HDR", "score": 500},
                {"format": 4, "name": "Neutral", "score": 0},
                {"format": 5, "name": "BR-DISK", "score": -10000}
            ]
        }))
        .expect("profile")
    }

    #[test]
    fn a_profile_summary_names_its_cutoff_group_and_allowed_qualities_in_order() {
        let summary = profile().summary(Some(9));
        assert_eq!(summary.cutoff.as_deref(), Some("UHD"));
        assert_eq!(
            summary.allowed_qualities,
            vec!["Bluray-1080p", "Bluray-2160p", "Remux-2160p"]
        );
        assert_eq!(summary.max_resolution, Some(2160));
        assert_eq!(
            summary.custom_format_scores,
            vec![
                FormatScore {
                    name: "HDR".to_owned(),
                    score: 500
                },
                FormatScore {
                    name: "BR-DISK".to_owned(),
                    score: -10000
                }
            ]
        );
        assert_eq!(summary.language.as_deref(), Some("Original"));
        assert!(summary.is_default);
        assert!(!profile().summary(Some(10)).is_default);
    }

    #[test]
    fn credentials_in_quoted_urls_are_masked() {
        assert_eq!(
            redact("Failed: http://10.0.0.1:9696/4/download?apikey=7f21de&link=abc failed"),
            "Failed: http://10.0.0.1:9696/4/download?apikey=<redacted>&link=abc failed"
        );
        assert_eq!(
            redact("magnet:?xt=urn:btih:x&tr=https://t.example/announce?passkey=s3cret"),
            "magnet:?xt=urn:btih:x&tr=https://t.example/announce?passkey=<redacted>"
        );
        assert_eq!(
            redact("Unable to connect to http://nzbget:hunter2@nzbget:6789/jsonrpc"),
            "Unable to connect to http://<redacted>@nzbget:6789/jsonrpc"
        );
        assert_eq!(
            redact("Star.Wars.1999.2160p.Remux-GRP"),
            "Star.Wars.1999.2160p.Remux-GRP"
        );
    }

    #[test]
    fn a_queue_record_carries_its_rejection_messages() {
        let record: QueueRecord = serde_json::from_value(json!({
            "id": 5, "movieId": 958, "title": "Some.Release",
            "trackedDownloadStatus": "warning", "trackedDownloadState": "importBlocked",
            "statusMessages": [
                {"title": "Some.Release.mkv", "messages": ["Not an upgrade for existing movie file"]},
                {"title": "No files found", "messages": []}
            ],
            "errorMessage": "Import blocked"
        }))
        .expect("record");
        let download = Download::from(&record);
        assert_eq!(download.item_id, Some(958));
        assert_eq!(
            download.messages,
            vec![
                "Some.Release.mkv: Not an upgrade for existing movie file",
                "No files found",
                "Import blocked"
            ]
        );
    }
}
