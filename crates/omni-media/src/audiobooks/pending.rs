//! Grabs held until the MyAnonamouse account has VIP.
//!
//! VIP torrents rank like any other release. MyAnonamouse refuses one to a
//! non-VIP account with a 406, which Prowlarr's download proxy reports as a
//! bare 500; Omni counts that as a VIP refusal only when Prowlarr's own log
//! holds MyAnonamouse's "not VIP" answer from the same moment
//! ([`vip_refusal`]). A confirmed refusal keeps the request as a
//! `media-audiobook-pending` row (one per release) with an action for the
//! owner. `AudiobookWatch` retries a waiting row every [`RETRY_EVERY_MS`],
//! at most one refusal per run (each refusal pauses the indexer in
//! Prowlarr), until it is grabbed, cancelled or [`EXPIRE_AFTER_MS`] passes;
//! calling `media_audiobook_grab` again retries at once.

use omni_store::cbor::Extra;
use omni_store::entity::{Entity, EntityOps as _, EntityWrite as _};
use omni_store::{DocOps as _, StoreError};
use serde::{Deserialize, Serialize};

use super::rank::words;
use super::{
    AudiobookRelease, Audiobooks, GrabOutcome, GrabRequest, HOUR_MS, LOG, contains_all, ttl,
};

/// A waiting request is retried this often.
pub const RETRY_EVERY_MS: i64 = 6 * HOUR_MS;
/// A request still waiting after this long is dropped.
pub const EXPIRE_AFTER_MS: i64 = 14 * 24 * HOUR_MS;
/// What the owner can do about a VIP refusal.
pub const VIP_ACTION: &str = "MyAnonamouse refused this VIP torrent because the account does \
     not have VIP. Buy VIP with bonus points on MyAnonamouse (Store, VIP); Omni retries this \
     grab every 6 hours for 14 days, or call media_audiobook_grab again once VIP is active.";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingStage {
    /// Waiting for VIP; retried on schedule.
    Waiting,
    /// A later grab went through (see `grab_id`).
    Resumed,
    Cancelled,
    /// Still refused after [`EXPIRE_AFTER_MS`].
    Expired,
}

/// A grab request held for VIP, keyed by release id.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudiobookPendingData {
    pub release_id: String,
    pub query: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub book: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    pub stage: PendingStage,
    pub created_at: i64,
    pub updated_at: i64,
    /// Refused or failed attempts so far.
    pub attempts: u32,
    pub next_attempt_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grab_id: Option<String>,
    /// The last attempt's outcome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Entity for AudiobookPendingData {
    const NAME: &'static str = "media-audiobook-pending";
    type Key = String;
    fn key(&self) -> String {
        self.release_id.clone()
    }
}

/// One held request, for `media_audiobooks_status`.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingStatus {
    /// The release id; `media_audiobook_cancel` takes it to stop waiting.
    pub pending_id: String,
    pub title: String,
    pub book: String,
    #[serde(skip)]
    pub safe_title: Option<String>,
    pub stage: PendingStage,
    pub attempts: u32,
    pub created_at: i64,
    pub next_attempt_at: Option<i64>,
    pub grab_id: Option<String>,
    pub action: Option<String>,
    pub detail: Option<String>,
}

/// One log entry from Prowlarr's `/api/v1/log`.
#[derive(Debug, Deserialize)]
pub struct LogRecord {
    pub time: Option<String>,
    pub message: Option<String>,
    pub exception: Option<String>,
}

/// Whether Prowlarr logged MyAnonamouse's VIP refusal between `from_ms` and
/// `to_ms` (inclusive, with five seconds of slack for clock rounding).
pub fn vip_refusal(records: &[LogRecord], from_ms: i64, to_ms: i64) -> bool {
    records.iter().any(|r| {
        let at = r
            .time
            .as_deref()
            .and_then(|t| t.parse::<jiff::Timestamp>().ok())
            .map(jiff::Timestamp::as_millisecond);
        let text = format!(
            "{} {}",
            r.message.as_deref().unwrap_or(""),
            r.exception.as_deref().unwrap_or("")
        );
        at.is_some_and(|at| at >= from_ms - 5_000 && at <= to_ms + 5_000)
            && text.contains("VIP torrent and you are not VIP")
    })
}

/// What one resume pass did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResumeReport {
    pub attempted: usize,
    pub resumed: usize,
    pub expired: usize,
}

impl Audiobooks {
    async fn pending_rows(&self) -> Result<Vec<AudiobookPendingData>, StoreError> {
        self.store
            .read(|tx| tx.get_all::<AudiobookPendingData>())
            .await
    }

    /// Records a confirmed VIP refusal: a new waiting row, or one more
    /// attempt on the waiting row for this release.
    pub(super) async fn hold_for_vip(&self, request: &GrabRequest, release: &AudiobookRelease) {
        let fresh = AudiobookPendingData {
            release_id: release.release_id.clone(),
            query: request.query.clone(),
            title: release.title.clone(),
            book: release.book.clone(),
            author: release.author.clone(),
            stage: PendingStage::Waiting,
            created_at: 0,
            updated_at: 0,
            attempts: 0,
            next_attempt_at: 0,
            grab_id: None,
            detail: None,
            extra: Extra::default(),
        };
        let held = self
            .store
            .write(move |tx| {
                let now = tx.now_ms();
                let mut row = match tx.get::<AudiobookPendingData>(&fresh.release_id)? {
                    Some(row) if row.stage == PendingStage::Waiting => row,
                    _ => AudiobookPendingData {
                        created_at: now,
                        ..fresh
                    },
                };
                row.attempts += 1;
                row.updated_at = now;
                row.next_attempt_at = now + RETRY_EVERY_MS;
                row.detail = Some("MyAnonamouse refused the VIP torrent".to_owned());
                tx.upsert(&row, ttl())
            })
            .await;
        if let Err(error) = held {
            tracing::warn!(target: LOG, "Audiobook pending record failed: {error}");
        }
    }

    /// Settles a waiting row for `release_id` once a grab got past the
    /// download (`grab_id` set) or found the book already there.
    pub(super) async fn settle_pending(&self, release_id: &str, grab_id: Option<String>) {
        let key = release_id.to_owned();
        let settled = self
            .store
            .write(move |tx| {
                let Some(mut row) = tx.get::<AudiobookPendingData>(&key)? else {
                    return Ok::<_, StoreError>(());
                };
                if row.stage != PendingStage::Waiting {
                    return Ok(());
                }
                row.stage = PendingStage::Resumed;
                row.updated_at = tx.now_ms();
                row.grab_id = grab_id;
                tx.upsert(&row, ttl())
            })
            .await;
        if let Err(error) = settled {
            tracing::warn!(target: LOG, "Audiobook pending record failed: {error}");
        }
    }

    /// Stops waiting for `release_id`; `None` when no row is waiting.
    pub(super) async fn cancel_pending(&self, release_id: &str) -> Option<String> {
        let key = release_id.to_owned();
        self.store
            .write(move |tx| {
                let Some(mut row) = tx.get::<AudiobookPendingData>(&key)? else {
                    return Ok::<_, StoreError>(None);
                };
                if row.stage != PendingStage::Waiting {
                    return Ok(None);
                }
                row.stage = PendingStage::Cancelled;
                row.updated_at = tx.now_ms();
                tx.upsert(&row, ttl())?;
                Ok(Some(row.display()))
            })
            .await
            .ok()
            .flatten()
    }

    /// Held requests, newest first, optionally filtered like grabs.
    pub(super) async fn pending_status(
        &self,
        wanted: Option<&[String]>,
    ) -> Result<Vec<PendingStatus>, StoreError> {
        let mut rows: Vec<AudiobookPendingData> = self
            .pending_rows()
            .await?
            .into_iter()
            .filter(|r| {
                wanted.is_none_or(|w| {
                    contains_all(&words(&format!("{} {}", r.display(), r.title)), w)
                        || (w.len() == 1 && r.release_id.starts_with(&w[0]))
                })
            })
            .collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.created_at));
        rows.truncate(super::STATUS_LIMIT);
        Ok(rows
            .into_iter()
            .map(|r| {
                let waiting = r.stage == PendingStage::Waiting;
                PendingStatus {
                    safe_title: r.book.clone(),
                    pending_id: r.release_id.clone(),
                    book: r.display(),
                    title: r.title,
                    stage: r.stage,
                    attempts: r.attempts,
                    created_at: r.created_at,
                    next_attempt_at: waiting.then_some(r.next_attempt_at),
                    grab_id: r.grab_id,
                    action: waiting.then(|| VIP_ACTION.to_owned()),
                    detail: r.detail,
                }
            })
            .collect())
    }

    /// Retries due waiting requests, stopping after the first VIP refusal
    /// of the run.
    pub async fn resume_pending(&self, now: i64) -> Result<ResumeReport, StoreError> {
        let mut report = ResumeReport::default();
        let mut due: Vec<AudiobookPendingData> = self
            .pending_rows()
            .await?
            .into_iter()
            .filter(|r| r.stage == PendingStage::Waiting && r.next_attempt_at <= now)
            .collect();
        due.sort_by_key(|r| r.next_attempt_at);
        for row in due {
            if now - row.created_at > EXPIRE_AFTER_MS {
                self.close_pending(&row.release_id, PendingStage::Expired, None)
                    .await;
                report.expired += 1;
                continue;
            }
            // A cancel since the read wins.
            let current = self
                .store
                .read({
                    let key = row.release_id.clone();
                    move |tx| tx.get::<AudiobookPendingData>(&key)
                })
                .await?;
            if current.is_none_or(|r| r.stage != PendingStage::Waiting) {
                continue;
            }
            report.attempted += 1;
            let summary = self
                .grab(GrabRequest {
                    query: row.query.clone(),
                    release_id: row.release_id.clone(),
                    force: false,
                })
                .await;
            match summary.outcome {
                // The grab settled the row itself.
                GrabOutcome::Grabbed
                | GrabOutcome::Uncertain
                | GrabOutcome::AlreadyHave
                | GrabOutcome::AlreadyGrabbed => {
                    tracing::info!(target: LOG, "Resumed audiobook {}", row.title);
                    report.resumed += 1;
                }
                GrabOutcome::AwaitingVip => break,
                _ => {
                    self.close_pending(
                        &row.release_id,
                        PendingStage::Waiting,
                        summary.reason.or(Some(format!("{:?}", summary.outcome))),
                    )
                    .await;
                }
            }
        }
        Ok(report)
    }

    /// Moves a waiting row to `stage`; a row kept waiting is pushed to its
    /// next attempt with `detail`.
    async fn close_pending(&self, release_id: &str, stage: PendingStage, detail: Option<String>) {
        let key = release_id.to_owned();
        let closed = self
            .store
            .write(move |tx| {
                let Some(mut row) = tx.get::<AudiobookPendingData>(&key)? else {
                    return Ok::<_, StoreError>(());
                };
                if row.stage != PendingStage::Waiting {
                    return Ok(());
                }
                let now = tx.now_ms();
                row.stage = stage;
                row.updated_at = now;
                if stage == PendingStage::Waiting {
                    row.attempts += 1;
                    row.next_attempt_at = now + RETRY_EVERY_MS;
                    row.detail = detail;
                }
                tx.upsert(&row, ttl())
            })
            .await;
        if let Err(error) = closed {
            tracing::warn!(target: LOG, "Audiobook pending record failed: {error}");
        }
    }
}

impl AudiobookPendingData {
    /// The book's name for people: the parsed title and author, else the release title.
    pub fn display(&self) -> String {
        match (&self.book, &self.author) {
            (Some(book), Some(author)) => format!("{book} by {author}"),
            (Some(book), None) => book.clone(),
            _ => self.title.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{LogRecord, vip_refusal};

    fn record(time: &str, message: &str) -> LogRecord {
        LogRecord {
            time: Some(time.to_owned()),
            message: Some(message.to_owned()),
            exception: None,
        }
    }

    #[test]
    fn a_vip_refusal_counts_only_inside_the_request_window() {
        let at = "2026-11-01T03:18:36Z"
            .parse::<jiff::Timestamp>()
            .expect("time")
            .as_millisecond();
        let refused = [record(
            "2026-11-01T03:18:36Z",
            "HTTP Error - Res: HTTP/2.0 [GET] <url>: 406.NotAcceptable (218 bytes)\n<html>\
             <body><h1>Download blocked: VIP torrent and you are not VIP or higher.</h1>",
        )];
        assert!(vip_refusal(&refused, at - 1_000, at + 500));
        assert!(!vip_refusal(&refused, at + 60_000, at + 61_000));
        let other = [record(
            "2026-11-01T03:18:36Z",
            "Downloading for release failed",
        )];
        assert!(!vip_refusal(&other, at - 1_000, at + 500));
    }
}
