//! Removing downloads from Radarr's or Sonarr's queue.
//!
//! A removal goes through the service, never straight to the download client:
//! `DELETE queue/{id}` with `removeFromClient=true` drops the download and its
//! files from NZBGet, optionally blocklists the release, and lets the service
//! search for a replacement only when asked (`skipRedownload=false`). Library
//! files are never touched.
//!
//! Each removal is reserved durably per service and queue id before the
//! request (Radarr and Sonarr derive queue ids from the download and the
//! title, so they survive restarts). A reserved or uncertain removal is never
//! sent again on its own: it settles only by reading the queue, and only
//! `force` sends it again while the download is still queued.

use std::collections::BTreeSet;

use omni_core::spawn::must_complete;
use omni_http::{Method, SideEffectMode};
use omni_store::cbor::Extra;
use omni_store::entity::{Entity, EntityOps as _, EntityWrite as _, UpsertOpts};
use omni_store::{Store, StoreError};
use serde::{Deserialize, Serialize};
use tokio_util::task::TaskTracker;

use super::inspect::{ArrInspect, ArrService, Download, InspectError, read, redact};
use super::{HttpResult, Limits, WriteOutcome};

const LOG: &str = "ArrQueue";
const REMOVAL_RECORD_TTL_MS: i64 = 7 * 24 * 60 * 60 * 1000;
/// The most recent blocklist entries read to confirm a blocklisting.
const BLOCKLIST_PAGE: usize = 50;
/// Clock skew allowed between Omni and the service when matching a blocklist
/// entry to this call.
const BLOCKLIST_SLACK_MS: i64 = 60_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemovalState {
    /// Written before the request; the request may have reached the service.
    Reserved,
    /// The service accepted the removal.
    Removed,
    /// A timeout, unreadable response or 5xx: the removal may have applied.
    Uncertain,
}

/// Durable per-download removal reservation, keyed by service and queue id.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadRemovalData {
    pub service: ArrService,
    pub queue_id: i64,
    pub state: RemovalState,
    pub requested_at: i64,
    pub blocklist: bool,
    pub redownload: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Entity for DownloadRemovalData {
    const NAME: &'static str = "media-download-removal";
    type Key = String;
    fn key(&self) -> String {
        removal_key(self.service, self.queue_id)
    }
}

fn removal_key(service: ArrService, queue_id: i64) -> String {
    format!("{}:{queue_id}", service.name().to_lowercase())
}

fn ttl() -> UpsertOpts {
    UpsertOpts {
        ttl_ms: Some(REMOVAL_RECORD_TTL_MS),
        ..UpsertOpts::default()
    }
}

#[derive(Clone, Debug)]
pub struct RemoveRequest {
    pub service: ArrService,
    pub queue_ids: BTreeSet<i64>,
    /// Blocklist the release so the service never grabs it again.
    pub blocklist: bool,
    /// Let the service search for a replacement (requires `blocklist`).
    pub redownload: bool,
    /// Send again although an earlier removal of this download was recorded.
    pub force: bool,
    pub dry_run: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoveOutcome {
    /// `dryRun`: the download would be removed.
    WouldRemove,
    /// `SideEffectMode::Record`: recorded instead of sent.
    Recorded,
    /// Removed, and the queue read afterwards no longer lists it.
    Removed,
    /// The service accepted the removal but still lists the download.
    StillQueued,
    /// The service accepted the removal; the queue could not be read back.
    Unverified,
    /// The request may have applied; the download is still listed. Never resent
    /// automatically.
    Uncertain,
    /// The service refused the removal.
    Rejected,
    /// An earlier removal through Omni is recorded and the download is still
    /// queued; `force` sends it again.
    AlreadyRequested,
    /// Gone from the queue after an earlier removal through Omni.
    AlreadyRemoved,
    /// No such download in the queue.
    NotFound,
    /// The service is importing it right now; removing it could break the import.
    Importing,
    /// The removal record could not be written, so nothing was sent.
    Failed,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemovedDownload {
    pub queue_id: i64,
    pub outcome: RemoveOutcome,
    /// The download as listed before the removal.
    pub download: Option<Download>,
    /// With `blocklist`: whether the service's blocklist now names the release;
    /// `null` when not asked for or unreadable.
    pub blocklisted: Option<bool>,
    /// When the earlier removal through Omni was asked for.
    pub requested_at: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoveSummary {
    pub service: ArrService,
    pub dry_run: bool,
    pub blocklist: bool,
    pub redownload: bool,
    pub downloads: Vec<RemovedDownload>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BlocklistRecord {
    #[serde(default)]
    movie_id: Option<i64>,
    #[serde(default)]
    series_id: Option<i64>,
    #[serde(default)]
    source_title: Option<String>,
    #[serde(default)]
    date: Option<String>,
}

impl BlocklistRecord {
    fn at_ms(&self) -> Option<i64> {
        self.date
            .as_deref()?
            .parse::<jiff::Timestamp>()
            .ok()
            .map(jiff::Timestamp::as_millisecond)
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
struct BlocklistPage {
    #[serde(default)]
    records: Vec<BlocklistRecord>,
}

/// Queue removals over both services. Removals are serialized and run to
/// completion even when the calling request is dropped.
#[derive(Clone)]
pub struct ArrQueue {
    inspect: ArrInspect,
    store: Store,
    tracker: TaskTracker,
    writes: std::sync::Arc<tokio::sync::Mutex<()>>,
}

fn store_failure(error: &StoreError) {
    tracing::warn!(target: LOG, "Download removal record failed: {error}");
}

/// Whether `queue` still lists the download.
fn listed(queue: &[Download], queue_id: i64, download_id: Option<&str>) -> bool {
    queue.iter().any(|d| {
        d.queue_id == Some(queue_id)
            || download_id.is_some_and(|id| d.download_id.as_deref() == Some(id))
    })
}

impl ArrQueue {
    pub fn new(inspect: ArrInspect, store: Store, tracker: TaskTracker) -> Self {
        Self {
            inspect,
            store,
            tracker,
            writes: std::sync::Arc::default(),
        }
    }

    pub async fn remove(&self, request: RemoveRequest) -> Result<RemoveSummary, InspectError> {
        let this = self.clone();
        must_complete(&self.tracker, async move {
            let _writes = this.writes.lock().await;
            this.remove_inner(request).await
        })
        .await
    }

    async fn remove_inner(&self, request: RemoveRequest) -> Result<RemoveSummary, InspectError> {
        let service = request.service;
        let connection = self.inspect.connection(service)?;
        let started = self.store.clock().now_ms();
        let queue = self.full_queue(service).await?;
        let mut results = Vec::new();
        // (index into results, what the request's response proved)
        let mut sent: Vec<(usize, WriteOutcome)> = Vec::new();
        // Download ids already sent in this call: the queue items of one
        // season pack share a download, and removing one removes them all.
        let mut sent_downloads: Vec<(String, WriteOutcome)> = Vec::new();
        for &queue_id in &request.queue_ids {
            let download = queue.iter().find(|d| d.queue_id == Some(queue_id)).cloned();
            let previous = match self.record(service, queue_id).await {
                Ok(previous) => previous,
                Err(error) => {
                    store_failure(&error);
                    results.push(item(queue_id, RemoveOutcome::Failed, download, None));
                    continue;
                }
            };
            let Some(download) = download else {
                let outcome = match &previous {
                    Some(previous) => {
                        if previous.state != RemovalState::Removed && !request.dry_run {
                            self.settle(previous.clone(), WriteOutcome::Accepted).await;
                        }
                        RemoveOutcome::AlreadyRemoved
                    }
                    None => RemoveOutcome::NotFound,
                };
                let at = previous.map(|p| p.requested_at);
                results.push(item(queue_id, outcome, None, at));
                continue;
            };
            if let Some(previous) = previous.as_ref().filter(|_| !request.force) {
                results.push(item(
                    queue_id,
                    RemoveOutcome::AlreadyRequested,
                    Some(download),
                    Some(previous.requested_at),
                ));
                continue;
            }
            if download.tracked_download_state.as_deref() == Some("importing") {
                results.push(item(
                    queue_id,
                    RemoveOutcome::Importing,
                    Some(download),
                    None,
                ));
                continue;
            }
            if request.dry_run {
                results.push(item(
                    queue_id,
                    RemoveOutcome::WouldRemove,
                    Some(download),
                    None,
                ));
                continue;
            }
            let path = format!(
                "queue/{queue_id}?removeFromClient=true&blocklist={}&skipRedownload={}&changeCategory=false",
                request.blocklist, !request.redownload
            );
            if self.inspect.http().mode() == SideEffectMode::Record {
                self.inspect
                    .http()
                    .record_write(connection.0, &path, serde_json::Value::Null);
                results.push(item(
                    queue_id,
                    RemoveOutcome::Recorded,
                    Some(download),
                    None,
                ));
                continue;
            }
            let row = DownloadRemovalData {
                service,
                queue_id,
                state: RemovalState::Reserved,
                requested_at: self.store.clock().now_ms(),
                blocklist: request.blocklist,
                redownload: request.redownload,
                download_id: download.download_id.clone(),
                title: download.title.clone(),
                extra: Extra::default(),
            };
            if let Err(error) = self
                .store
                .write({
                    let row = row.clone();
                    move |tx| tx.upsert(&row, ttl())
                })
                .await
            {
                store_failure(&error);
                results.push(item(queue_id, RemoveOutcome::Failed, Some(download), None));
                continue;
            }
            let sibling = download.download_id.as_ref().and_then(|id| {
                sent_downloads
                    .iter()
                    .find(|(sent, _)| sent == id)
                    .map(|(_, written)| *written)
            });
            let written = match sibling {
                Some(written) => written,
                None => {
                    let response = self
                        .inspect
                        .http()
                        .send_unit(connection, Method::DELETE, &path, Limits::DEFAULT)
                        .await;
                    match response {
                        // No such queue item: it may have just gone, so the
                        // read-back decides.
                        HttpResult::HttpError { status_code: 404 } => WriteOutcome::Uncertain,
                        response => WriteOutcome::of(&response),
                    }
                }
            };
            if let Some(id) = download.download_id.clone()
                && sibling.is_none()
                && written != WriteOutcome::Rejected
            {
                sent_downloads.push((id, written));
            }
            match written {
                WriteOutcome::Rejected => {
                    tracing::warn!(
                        target: LOG,
                        "{} refused to remove queue item {queue_id}",
                        service.name()
                    );
                    self.release(row, previous).await;
                    results.push(item(
                        queue_id,
                        RemoveOutcome::Rejected,
                        Some(download),
                        None,
                    ));
                }
                WriteOutcome::Accepted | WriteOutcome::Uncertain => {
                    self.settle(row, written).await;
                    sent.push((results.len(), written));
                    results.push(item(queue_id, RemoveOutcome::Removed, Some(download), None));
                }
            }
        }
        if !sent.is_empty() {
            self.verify(&request, &mut results, &sent, started - BLOCKLIST_SLACK_MS)
                .await;
        }
        Ok(RemoveSummary {
            service,
            dry_run: request.dry_run,
            blocklist: request.blocklist,
            redownload: request.redownload,
            downloads: results,
        })
    }

    /// Reads the queue and the blocklist back after the removals were sent.
    async fn verify(
        &self,
        request: &RemoveRequest,
        results: &mut [RemovedDownload],
        sent: &[(usize, WriteOutcome)],
        since: i64,
    ) {
        let service = request.service;
        let after = self.full_queue(service).await.ok();
        let blocklist = if request.blocklist {
            self.blocklist(service).await
        } else {
            None
        };
        for &(index, written) in sent {
            let Some(result) = results.get_mut(index) else {
                continue;
            };
            let download_id = result.download.as_ref().and_then(|d| d.download_id.clone());
            let still = after
                .as_deref()
                .map(|queue| listed(queue, result.queue_id, download_id.as_deref()));
            result.outcome = match (written, still) {
                (_, Some(false)) => RemoveOutcome::Removed,
                (WriteOutcome::Accepted, Some(true)) => RemoveOutcome::StillQueued,
                (WriteOutcome::Accepted, None) => RemoveOutcome::Unverified,
                _ => RemoveOutcome::Uncertain,
            };
            if written == WriteOutcome::Uncertain
                && still == Some(false)
                && let Ok(Some(row)) = self.record(service, result.queue_id).await
            {
                self.settle(row, WriteOutcome::Accepted).await;
            }
            if result.outcome == RemoveOutcome::Removed {
                result.blocklisted = blocklist.as_ref().map(|records| {
                    let download = result.download.as_ref();
                    let title = download.and_then(|d| d.title.as_deref());
                    let item_id = download.and_then(|d| d.item_id);
                    title.is_some()
                        && records.iter().any(|r| {
                            r.source_title.as_deref().map(redact).as_deref() == title
                                && item_id.is_none_or(|id| r.movie_id.or(r.series_id) == Some(id))
                                && r.at_ms().is_some_and(|at| at >= since)
                        })
                });
            }
            tracing::info!(
                target: LOG,
                "Removal of {} queue item {}: {:?}",
                service.name(),
                result.queue_id,
                result.outcome
            );
        }
    }

    /// The whole queue; a queue longer than one page fails rather than
    /// deciding from part of it.
    async fn full_queue(&self, service: ArrService) -> Result<Vec<Download>, InspectError> {
        let queue = self.inspect.queue(service).await?;
        if queue.truncated {
            return Err(InspectError::QueueTruncated(service.name()));
        }
        Ok(queue.downloads)
    }

    async fn blocklist(&self, service: ArrService) -> Option<Vec<BlocklistRecord>> {
        let connection = self.inspect.connection(service).ok()?;
        let path = format!(
            "blocklist?page=1&pageSize={BLOCKLIST_PAGE}&sortKey=date&sortDirection=descending"
        );
        read(
            service,
            self.inspect
                .http()
                .send_limited::<BlocklistPage>(
                    connection,
                    Method::GET,
                    &path,
                    None,
                    Limits::DEFAULT,
                )
                .await,
        )
        .ok()
        .map(|page| page.records)
    }

    async fn record(
        &self,
        service: ArrService,
        queue_id: i64,
    ) -> Result<Option<DownloadRemovalData>, StoreError> {
        let key = removal_key(service, queue_id);
        self.store
            .read(move |tx| tx.get::<DownloadRemovalData>(&key))
            .await
    }

    async fn settle(&self, row: DownloadRemovalData, written: WriteOutcome) {
        let state = match written {
            WriteOutcome::Accepted => RemovalState::Removed,
            WriteOutcome::Uncertain | WriteOutcome::Rejected => RemovalState::Uncertain,
        };
        let row = DownloadRemovalData { state, ..row };
        if let Err(error) = self.store.write(move |tx| tx.upsert(&row, ttl())).await {
            // The reservation stays and keeps blocking a repeat.
            store_failure(&error);
        }
    }

    /// A refused removal restores the record its reservation replaced.
    async fn release(&self, row: DownloadRemovalData, previous: Option<DownloadRemovalData>) {
        let released = self
            .store
            .write(move |tx| match previous {
                Some(previous) => tx.upsert(&previous, ttl()),
                None => tx.delete::<DownloadRemovalData>(&row.key()).map(|_| ()),
            })
            .await;
        if let Err(error) = released {
            store_failure(&error);
        }
    }
}

fn item(
    queue_id: i64,
    outcome: RemoveOutcome,
    download: Option<Download>,
    requested_at: Option<i64>,
) -> RemovedDownload {
    RemovedDownload {
        queue_id,
        outcome,
        download,
        blocklisted: None,
        requested_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn download(queue_id: i64, download_id: &str) -> Download {
        let record: super::super::inspect::QueueRecord =
            serde_json::from_value(serde_json::json!({"id": queue_id, "downloadId": download_id}))
                .expect("record");
        Download::from(&record)
    }

    #[test]
    fn a_download_is_still_listed_under_its_queue_id_or_its_client_id() {
        let queue = vec![download(5, "abc"), download(6, "def")];
        assert!(listed(&queue, 5, None));
        assert!(
            listed(&queue, 9, Some("def")),
            "a season pack keeps its download id"
        );
        assert!(!listed(&queue, 9, Some("xyz")));
        assert!(!listed(&queue, 9, None));
    }

    #[test]
    fn removal_keys_name_the_service_and_queue_id() {
        assert_eq!(removal_key(ArrService::Radarr, 77), "radarr:77");
        assert_eq!(removal_key(ArrService::Sonarr, 77), "sonarr:77");
    }
}
