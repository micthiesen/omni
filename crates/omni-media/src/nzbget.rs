//! Read-only NZBGet status (`NZBGET_URL`, its JSON-RPC API on the LAN).
//!
//! Radarr and Sonarr show a download's progress but not NZBGet itself: a
//! paused queue (NZBGet pauses itself when disk space runs low), news-server
//! trouble, the post-processing stage of each download, and the exact failure
//! status of finished ones. Only read methods are called (`version`, `status`,
//! `listgroups`, `history`, `log`); nothing here changes NZBGet. Removing a
//! download goes through Radarr or Sonarr (`arr::queue`), which keeps their
//! queues and blocklists consistent.
//!
//! History rows carry the NZB's source `URL`, which can hold an indexer API
//! key; it is never read. Names and log text pass `inspect::redact`.

use std::time::Duration;

use omni_http::{HttpClient, Method, Url};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::arr::inspect::redact;

const LOG: &str = "NZBGet";
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
/// `history` returns every retained row (30 days by default) with its
/// per-server statistics.
const HISTORY_MAX_BYTES: usize = 48 * 1024 * 1024;
/// Duplicate handling, not failures.
const DUPLICATE_STATUSES: [&str; 3] = ["DELETED/DUPE", "DELETED/COPY", "DELETED/GOOD"];
const TIMEOUT: Duration = Duration::from_secs(20);
/// Log lines scanned for warnings and errors.
const LOG_TAIL: u32 = 200;
/// The most of each list one status returns.
pub const DOWNLOAD_LIMIT: usize = 50;
pub const HISTORY_LIMIT: usize = 20;
pub const MESSAGE_LIMIT: usize = 25;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NzbGetError {
    #[error("NZBGet is not configured")]
    NotConfigured,
    #[error("NZBGet did not answer within {0} s")]
    Timeout(u64),
    #[error("NZBGet {method} failed: {cause}")]
    Request {
        method: &'static str,
        cause: &'static str,
    },
    #[error("NZBGet {method} answered HTTP {status_code}")]
    Http {
        method: &'static str,
        status_code: u16,
    },
}

#[derive(Deserialize)]
struct RpcResult<T> {
    result: T,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct NewsServerResource {
    #[serde(rename = "ID")]
    id: i64,
    #[serde(default)]
    active: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct StatusResource {
    #[serde(rename = "RemainingSizeMB")]
    remaining_size_mb: i64,
    download_rate: f64,
    average_download_rate: f64,
    download_limit: f64,
    post_job_count: i64,
    download_paused: bool,
    post_paused: bool,
    scan_paused: bool,
    server_stand_by: bool,
    quota_reached: bool,
    #[serde(rename = "FreeDiskSpaceMB")]
    free_disk_space_mb: Option<i64>,
    #[serde(rename = "TotalDiskSpaceMB")]
    total_disk_space_mb: Option<i64>,
    #[serde(rename = "FreeInterDiskSpaceMB")]
    free_inter_disk_space_mb: Option<i64>,
    #[serde(rename = "TotalInterDiskSpaceMB")]
    total_inter_disk_space_mb: Option<i64>,
    resume_time: i64,
    news_servers: Vec<NewsServerResource>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Parameter {
    #[serde(default)]
    name: String,
    #[serde(default)]
    value: String,
}

fn drone(parameters: &[Parameter]) -> Option<String> {
    parameters
        .iter()
        .find(|p| p.name == "drone")
        .map(|p| p.value.clone())
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct GroupResource {
    #[serde(rename = "NZBID")]
    nzb_id: i64,
    #[serde(rename = "NZBName")]
    nzb_name: String,
    category: String,
    status: String,
    #[serde(rename = "FileSizeMB")]
    file_size_mb: i64,
    #[serde(rename = "RemainingSizeMB")]
    remaining_size_mb: i64,
    health: i64,
    critical_health: i64,
    post_info_text: String,
    post_stage_progress: i64,
    active_downloads: i64,
    parameters: Vec<Parameter>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct HistoryResource {
    #[serde(rename = "NZBID")]
    nzb_id: i64,
    name: String,
    category: String,
    status: String,
    par_status: String,
    unpack_status: String,
    move_status: String,
    delete_status: String,
    mark_status: String,
    health: i64,
    #[serde(rename = "FileSizeMB")]
    file_size_mb: i64,
    history_time: i64,
    parameters: Vec<Parameter>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct LogResource {
    kind: String,
    time: i64,
    text: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NewsServer {
    pub id: i64,
    pub active: bool,
}

/// A download in NZBGet's queue.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NzbGetDownload {
    pub nzb_id: i64,
    /// Radarr's or Sonarr's download id (`downloadId` in media_downloads_list).
    pub download_id: Option<String>,
    pub name: String,
    pub category: String,
    /// For example QUEUED, PAUSED, DOWNLOADING, PP_QUEUED, REPAIRING, UNPACKING, MOVING.
    pub status: String,
    pub size_mb: i64,
    pub remaining_mb: i64,
    /// Share of articles still available, in percent.
    pub health_percent: f64,
    /// Below this health the download cannot be repaired.
    pub critical_health_percent: f64,
    /// The running post-processing step, for example "Unpacking".
    pub post_stage: Option<String>,
    pub post_stage_percent: Option<f64>,
    pub active_downloads: i64,
}

/// A finished download that did not succeed.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NzbGetFailure {
    pub nzb_id: i64,
    pub download_id: Option<String>,
    pub name: String,
    pub category: String,
    /// For example FAILURE/HEALTH, FAILURE/UNPACK, FAILURE/PASSWORD, WARNING/DAMAGED, DELETED/MANUAL.
    pub status: String,
    pub par_status: String,
    pub unpack_status: String,
    pub move_status: String,
    pub delete_status: String,
    pub mark_status: String,
    pub health_percent: f64,
    pub size_mb: i64,
    pub finished_at: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NzbGetMessage {
    /// WARNING or ERROR.
    pub kind: String,
    pub at: i64,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NzbGetStatus {
    pub version: Option<String>,
    pub download_paused: bool,
    /// Download is paused and NZBGet's recent log reports low disk space.
    pub paused_for_disk_space: bool,
    pub post_processing_paused: bool,
    pub scan_paused: bool,
    pub quota_reached: bool,
    /// Nothing is downloading.
    pub idle: bool,
    /// When a timed pause ends (Unix ms).
    pub resume_at: Option<i64>,
    pub download_rate_bytes_per_sec: f64,
    pub average_download_rate_bytes_per_sec: f64,
    /// `null`: unlimited.
    pub download_limit_bytes_per_sec: Option<f64>,
    pub remaining_mb: i64,
    pub post_processing_jobs: i64,
    pub free_disk_space_mb: Option<i64>,
    pub total_disk_space_mb: Option<i64>,
    /// The intermediate (incomplete downloads) directory.
    pub free_inter_disk_space_mb: Option<i64>,
    pub total_inter_disk_space_mb: Option<i64>,
    pub news_servers: Vec<NewsServer>,
    pub downloads: Vec<NzbGetDownload>,
    pub total_downloads: usize,
    /// Newest first.
    pub failures: Vec<NzbGetFailure>,
    pub total_failures: usize,
    /// Warnings and errors among NZBGet's last log lines, newest first.
    pub messages: Vec<NzbGetMessage>,
    /// Reads that failed (`history`, `log`); their lists are empty, not complete.
    pub unreadable: Vec<String>,
}

fn percent(per_mille: i64) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let value = per_mille as f64;
    value / 10.0
}

/// The configured NZBGet, or none.
#[derive(Clone, Debug)]
pub struct NzbGet {
    http: HttpClient,
    url: Option<String>,
}

impl NzbGet {
    pub fn new(http: HttpClient, url: Option<String>) -> Self {
        Self {
            http,
            url: url.filter(|u| !u.is_empty()),
        }
    }

    pub async fn status(&self) -> Result<NzbGetStatus, NzbGetError> {
        let url = self.url.as_deref().ok_or(NzbGetError::NotConfigured)?;
        tokio::time::timeout(TIMEOUT, self.read(url))
            .await
            .unwrap_or(Err(NzbGetError::Timeout(TIMEOUT.as_secs())))
    }

    async fn read(&self, url: &str) -> Result<NzbGetStatus, NzbGetError> {
        let (version, status, groups, history, log) = futures::join!(
            self.rpc::<String>(url, "version", json!([]), MAX_RESPONSE_BYTES),
            self.rpc::<StatusResource>(url, "status", json!([]), MAX_RESPONSE_BYTES),
            self.rpc::<Vec<GroupResource>>(url, "listgroups", json!([0]), MAX_RESPONSE_BYTES),
            self.rpc::<Vec<HistoryResource>>(url, "history", json!([false]), HISTORY_MAX_BYTES),
            self.rpc::<Vec<LogResource>>(url, "log", json!([0, LOG_TAIL]), MAX_RESPONSE_BYTES),
        );
        let status = status?;
        let groups = groups?;
        let mut unreadable = Vec::new();
        let history = history.unwrap_or_else(|error| {
            tracing::warn!(target: LOG, "{error}");
            unreadable.push("history".to_owned());
            Vec::new()
        });
        let log = log.unwrap_or_else(|error| {
            tracing::warn!(target: LOG, "{error}");
            unreadable.push("log".to_owned());
            Vec::new()
        });
        let mut failures: Vec<HistoryResource> = history
            .into_iter()
            .filter(|h| {
                !h.status.starts_with("SUCCESS") && !DUPLICATE_STATUSES.contains(&h.status.as_str())
            })
            .collect();
        failures.sort_by_key(|h| std::cmp::Reverse(h.history_time));
        let mut messages: Vec<NzbGetMessage> = log
            .into_iter()
            .filter(|l| l.kind == "WARNING" || l.kind == "ERROR")
            .map(|l| NzbGetMessage {
                kind: l.kind,
                at: l.time.saturating_mul(1000),
                text: redact(&l.text),
            })
            .collect();
        messages.reverse();
        let low_disk = messages
            .iter()
            .any(|m| m.text.to_lowercase().contains("low disk space"));
        let total_downloads = groups.len();
        let total_failures = failures.len();
        messages.truncate(MESSAGE_LIMIT);
        Ok(NzbGetStatus {
            version: version.ok(),
            download_paused: status.download_paused,
            paused_for_disk_space: status.download_paused && low_disk,
            post_processing_paused: status.post_paused,
            scan_paused: status.scan_paused,
            quota_reached: status.quota_reached,
            idle: status.server_stand_by,
            resume_at: (status.resume_time > 0).then(|| status.resume_time.saturating_mul(1000)),
            download_rate_bytes_per_sec: status.download_rate,
            average_download_rate_bytes_per_sec: status.average_download_rate,
            download_limit_bytes_per_sec: (status.download_limit > 0.0)
                .then_some(status.download_limit),
            remaining_mb: status.remaining_size_mb,
            post_processing_jobs: status.post_job_count,
            free_disk_space_mb: status.free_disk_space_mb,
            total_disk_space_mb: status.total_disk_space_mb,
            free_inter_disk_space_mb: status.free_inter_disk_space_mb,
            total_inter_disk_space_mb: status.total_inter_disk_space_mb,
            news_servers: status
                .news_servers
                .iter()
                .map(|s| NewsServer {
                    id: s.id,
                    active: s.active,
                })
                .collect(),
            downloads: groups
                .iter()
                .take(DOWNLOAD_LIMIT)
                .map(|g| NzbGetDownload {
                    nzb_id: g.nzb_id,
                    download_id: drone(&g.parameters),
                    name: redact(&g.nzb_name),
                    category: g.category.clone(),
                    status: g.status.clone(),
                    size_mb: g.file_size_mb,
                    remaining_mb: g.remaining_size_mb,
                    health_percent: percent(g.health),
                    critical_health_percent: percent(g.critical_health),
                    post_stage: Some(redact(&g.post_info_text)).filter(|t| !t.is_empty()),
                    post_stage_percent: (!g.post_info_text.is_empty())
                        .then(|| percent(g.post_stage_progress)),
                    active_downloads: g.active_downloads,
                })
                .collect(),
            total_downloads,
            failures: failures
                .iter()
                .take(HISTORY_LIMIT)
                .map(|h| NzbGetFailure {
                    nzb_id: h.nzb_id,
                    download_id: drone(&h.parameters),
                    name: redact(&h.name),
                    category: h.category.clone(),
                    status: h.status.clone(),
                    par_status: h.par_status.clone(),
                    unpack_status: h.unpack_status.clone(),
                    move_status: h.move_status.clone(),
                    delete_status: h.delete_status.clone(),
                    mark_status: h.mark_status.clone(),
                    health_percent: percent(h.health),
                    size_mb: h.file_size_mb,
                    finished_at: h.history_time.saturating_mul(1000),
                })
                .collect(),
            total_failures,
            messages,
            unreadable,
        })
    }

    async fn rpc<T: DeserializeOwned>(
        &self,
        base: &str,
        method: &'static str,
        params: Value,
        max_bytes: usize,
    ) -> Result<T, NzbGetError> {
        let fail = |cause| NzbGetError::Request { method, cause };
        let url = Url::parse(&format!("{}/", base.trim_end_matches('/')))
            .and_then(|base| base.join("jsonrpc"))
            .map_err(|_| fail("invalid NZBGET_URL"))?;
        let body = serde_json::to_vec(&json!({ "method": method, "params": params, "id": 1 }))
            .map_err(|_| fail("request failed"))?;
        let response = self
            .http
            .request(Method::POST, url)
            .header("Content-Type", "application/json")
            .body(body)
            .send_bounded(max_bytes)
            .await
            .map_err(|error| {
                // The error can quote the URL, and NZBGET_URL may carry credentials.
                tracing::debug!(target: LOG, "NZBGet {method} failed: {}", redact(&error.to_string()));
                match error {
                    omni_http::HttpError::TooLarge { .. } => fail("response too large"),
                    _ => fail("request failed"),
                }
            })?;
        if !response.status.is_success() {
            return Err(NzbGetError::Http {
                method,
                status_code: response.status.as_u16(),
            });
        }
        serde_json::from_slice::<RpcResult<T>>(&response.body)
            .map(|decoded| decoded.result)
            .map_err(|error| {
                tracing::debug!(target: LOG, "NZBGet {method} response rejected: {error}");
                fail("unexpected response")
            })
    }
}
