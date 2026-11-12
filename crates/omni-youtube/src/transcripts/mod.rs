//! Transcripts of YouTube videos, made on this host: `yt-dlp` downloads the
//! audio track, `ffmpeg` decodes it two minutes at a time and the local
//! speech model (the [`SpeechTranscriber`] port of livestream intelligence)
//! turns it into timestamped text. See "Transcripts" in docs/youtube.md.
//!
//! One job runs at a time. The audio lives in the job's work directory and is
//! deleted when the job ends, however it ends; the directory is also emptied
//! at boot. A job cut short by a shutdown resumes at the next boot from the
//! speech it had already stored. Transcript text is kept [`RETENTION_MS`]
//! after its last read.

pub mod media;
pub mod model;
pub mod text;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use futures::future::BoxFuture;
use omni_core::clock::SharedClock;
use omni_core::spawn::spawn_tracked;
use omni_http::SideEffectMode;
use omni_runtime::ports::{Ports, SPEECH_SAMPLE_RATE, SpeechTranscriber};
use omni_store::cbor::Extra;
use omni_store::entity::{EntityOps as _, EntityWrite as _, UpsertOpts};
use omni_store::{Store, StoreError};
use omni_tasks::{CronSchedule, RunContext, Task, TaskError, TaskOptions};
use tokio::sync::{Semaphore, watch};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::records::{
    TranscriptGapData, TranscriptSegmentData, TranscriptState, YoutubeTranscriptData,
};
use crate::transcripts::media::{MediaError, MediaSource};

const LOG: &str = "YouTubeTranscripts";

/// A transcript is deleted this long after its last read (or, never read,
/// after its job ended).
pub const RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1000;
/// Finished transcripts kept at most; the least recently read go first.
pub const MAX_STORED: usize = 200;
/// Longer videos are refused.
pub const MAX_DURATION_MS: i64 = 6 * 60 * 60 * 1000;
/// Jobs queued or running at most.
pub const MAX_ACTIVE: usize = 5;
/// A job restarted by shutdowns this many times fails instead of resuming.
pub const MAX_ATTEMPTS: u32 = 3;
/// Audio decoded and transcribed per step; progress is stored after each.
pub const CHUNK_MS: i64 = 120_000;
/// The work directory under the data directory.
pub const WORK_DIR: &str = "youtube-transcripts";
/// A read refreshes `last_read_at` at most this often.
const TOUCH_INTERVAL_MS: i64 = 60 * 60 * 1000;

pub const CLEANUP_TASK: &str = "YoutubeTranscriptCleanup";
pub const CLEANUP_SCHEDULE: &str = "23 4 * * *";

/// Why a transcript tool could not answer.
#[derive(Debug, thiserror::Error)]
pub enum TranscriptError {
    #[error("{0}")]
    Unavailable(String),
    #[error("No transcript is stored for video {0}; start one with youtube_transcript_start")]
    NotFound(String),
    #[error(
        "{MAX_ACTIVE} transcripts are already queued or running; wait for one to finish (youtube_transcripts_list shows them)"
    )]
    Busy,
    #[error("transcript storage failed: {0}")]
    Store(#[from] StoreError),
}

impl TranscriptError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unavailable(_) => "unavailable",
            Self::NotFound(_) => "not_found",
            Self::Busy => "busy",
            Self::Store(_) => "internal",
        }
    }
}

/// How a job ended, other than with a transcript.
#[derive(Debug)]
enum JobError {
    /// Recorded on the row as `failed`.
    Failed {
        code: &'static str,
        message: String,
    },
    /// Shutdown: the row stays active and the next boot resumes it.
    Interrupted,
    Store(StoreError),
}

impl From<StoreError> for JobError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

fn failed(code: &'static str, message: impl Into<String>) -> JobError {
    JobError::Failed {
        code,
        message: message.into(),
    }
}

pub struct TranscriptDeps {
    pub store: Store,
    pub clock: SharedClock,
    pub ports: Ports,
    pub media: Arc<dyn MediaSource>,
    /// Holds one directory per running job and nothing else.
    pub work_dir: PathBuf,
    pub tracker: TaskTracker,
    pub shutdown: CancellationToken,
    pub mode: SideEffectMode,
}

struct Inner {
    deps: TranscriptDeps,
    /// One job downloads and transcribes at a time.
    slot: Semaphore,
    /// Videos with a job task in this process.
    running: Mutex<HashSet<String>>,
    /// Bumped whenever a row changes, for waiting readers.
    changed: watch::Sender<u64>,
}

/// What one cleanup pass removed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CleanupReport {
    pub expired: usize,
    pub over_limit: usize,
    pub stale_directories: usize,
}

#[derive(Clone)]
pub struct Transcripts {
    inner: Arc<Inner>,
}

fn samples_to_ms(samples: usize) -> i64 {
    i64::try_from(samples).unwrap_or(i64::MAX) * 1000 / i64::from(SPEECH_SAMPLE_RATE)
}

impl Transcripts {
    pub fn new(deps: TranscriptDeps) -> Self {
        Self {
            inner: Arc::new(Inner {
                deps,
                slot: Semaphore::new(1),
                running: Mutex::new(HashSet::new()),
                changed: watch::Sender::new(0),
            }),
        }
    }

    fn now(&self) -> i64 {
        self.inner.deps.clock.now_ms()
    }

    fn running(&self) -> MutexGuard<'_, HashSet<String>> {
        self.inner
            .running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn bump(&self) {
        self.inner.changed.send_modify(|n| *n = n.wrapping_add(1));
    }

    fn speech(&self) -> Option<Arc<dyn SpeechTranscriber>> {
        self.inner.deps.ports.speech_transcriber()
    }

    /// Whether new transcripts can start.
    pub fn speech_available(&self) -> bool {
        self.speech().is_some() && self.inner.deps.mode == SideEffectMode::Live
    }

    async fn read(&self, video_id: &str) -> Result<Option<YoutubeTranscriptData>, StoreError> {
        let key = video_id.to_owned();
        self.inner
            .deps
            .store
            .read(move |tx| tx.get::<YoutubeTranscriptData>(&key))
            .await
    }

    /// Read-modify-write of one row; `None` when it no longer exists.
    async fn update(
        &self,
        video_id: &str,
        change: impl FnOnce(&mut YoutubeTranscriptData) + Send + 'static,
    ) -> Result<Option<YoutubeTranscriptData>, StoreError> {
        let key = video_id.to_owned();
        let row = self
            .inner
            .deps
            .store
            .write(move |tx| {
                let Some(mut row) = tx.get::<YoutubeTranscriptData>(&key)? else {
                    return Ok::<_, StoreError>(None);
                };
                change(&mut row);
                tx.upsert(&row, UpsertOpts::default())?;
                Ok(Some(row))
            })
            .await?;
        self.bump();
        Ok(row)
    }

    /// Starts a transcript job for the video, or returns the one it already
    /// has: a running job is never restarted and a finished transcript is
    /// reused unless `force`. A failed one starts over.
    pub async fn start(
        &self,
        video_id: &str,
        force: bool,
    ) -> Result<YoutubeTranscriptData, TranscriptError> {
        if self.inner.deps.mode != SideEffectMode::Live {
            return Err(TranscriptError::Unavailable(
                "Transcripts are not made in preview mode".to_owned(),
            ));
        }
        if self.speech().is_none() {
            return Err(TranscriptError::Unavailable(
                "The local speech models are not loaded (livestream intelligence is disabled or failed to start), so no transcript can be made".to_owned(),
            ));
        }
        // The id names the job's work directory.
        if video_id.is_empty()
            || !video_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(TranscriptError::Unavailable(format!(
                "{video_id:?} is not a video id"
            )));
        }
        // A request dropped between storing the row and spawning its job
        // would leave a queued row that nothing runs.
        let this = self.clone();
        let id = video_id.to_owned();
        omni_core::spawn::must_complete(&self.inner.deps.tracker, async move {
            this.reserve(&id, force).await
        })
        .await
    }

    async fn reserve(
        &self,
        video_id: &str,
        force: bool,
    ) -> Result<YoutubeTranscriptData, TranscriptError> {
        let now = self.now();
        let key = video_id.to_owned();
        let row = self
            .inner
            .deps
            .store
            .write(move |tx| {
                if let Some(mut row) = tx.get::<YoutubeTranscriptData>(&key)? {
                    if row.state.is_active() {
                        return Ok(row);
                    }
                    if row.state == TranscriptState::Ready && !force {
                        row.last_read_at = now;
                        tx.upsert(&row, UpsertOpts::default())?;
                        return Ok(row);
                    }
                }
                let active = tx
                    .get_all::<YoutubeTranscriptData>()?
                    .iter()
                    .filter(|row| row.state.is_active())
                    .count();
                if active >= MAX_ACTIVE {
                    return Err(TranscriptError::Busy);
                }
                let row = YoutubeTranscriptData {
                    video_id: key,
                    state: TranscriptState::Queued,
                    title: None,
                    channel: None,
                    duration_ms: None,
                    upload_date: None,
                    requested_at: now,
                    started_at: None,
                    finished_at: None,
                    last_read_at: now,
                    transcribed_ms: 0,
                    attempts: 0,
                    segments: Vec::new(),
                    gaps: Vec::new(),
                    speech_ms: Some(0),
                    model: None,
                    error_code: None,
                    error: None,
                    extra: Extra::default(),
                };
                tx.upsert(&row, UpsertOpts::default())?;
                Ok(row)
            })
            .await?;
        if row.state.is_active() {
            self.ensure_running(video_id);
        }
        self.bump();
        Ok(row)
    }

    /// The stored transcript. Reading a finished one keeps it from expiring.
    pub async fn get(&self, video_id: &str) -> Result<YoutubeTranscriptData, TranscriptError> {
        let row = self
            .read(video_id)
            .await?
            .ok_or_else(|| TranscriptError::NotFound(video_id.to_owned()))?;
        let now = self.now();
        if row.state == TranscriptState::Ready && now - row.last_read_at >= TOUCH_INTERVAL_MS {
            let touched = self
                .update(video_id, move |row| row.last_read_at = now)
                .await?;
            return Ok(touched.unwrap_or(row));
        }
        Ok(row)
    }

    /// [`Transcripts::get`], waiting up to `wait` for a running job to end.
    pub async fn wait(
        &self,
        video_id: &str,
        wait: Duration,
    ) -> Result<YoutubeTranscriptData, TranscriptError> {
        let mut changed = self.inner.changed.subscribe();
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            changed.borrow_and_update();
            let row = self.get(video_id).await?;
            if !row.state.is_active() {
                return Ok(row);
            }
            match tokio::time::timeout_at(deadline, changed.changed()).await {
                Ok(Ok(())) => {}
                // Timed out, or the service is gone.
                _ => return Ok(row),
            }
        }
    }

    /// Stored transcripts and jobs, most recently requested first, with their total.
    pub async fn list(
        &self,
        limit: usize,
    ) -> Result<(Vec<YoutubeTranscriptData>, usize), TranscriptError> {
        let mut rows = self
            .inner
            .deps
            .store
            .read(|tx| tx.get_all::<YoutubeTranscriptData>())
            .await?;
        let total = rows.len();
        rows.sort_by(|a, b| {
            b.requested_at
                .cmp(&a.requested_at)
                .then_with(|| a.video_id.cmp(&b.video_id))
        });
        rows.truncate(limit);
        Ok((rows, total))
    }

    /// Spawns the video's job unless this process already runs it.
    fn ensure_running(&self, video_id: &str) {
        if !self.running().insert(video_id.to_owned()) {
            return;
        }
        let this = self.clone();
        let id = video_id.to_owned();
        spawn_tracked(&self.inner.deps.tracker, "youtube-transcript", async move {
            // Removes the video from `running` even if the job panics.
            struct Running(Transcripts, String);
            impl Drop for Running {
                fn drop(&mut self) {
                    self.0.running().remove(&self.1);
                }
            }
            let guard = Running(this.clone(), id.clone());
            let settled = this.job(&id).await;
            drop(guard);
            // A new request can replace the row between the job's last
            // write and the guard's release; it found the video running
            // and spawned nothing.
            if settled
                && !this.inner.deps.shutdown.is_cancelled()
                && matches!(this.read(&id).await, Ok(Some(row)) if row.state.is_active())
            {
                this.ensure_running(&id);
            }
        });
    }

    /// Runs the video's job. `false` when its outcome could not be stored
    /// (a shutdown or a failing store): the caller must not run it again.
    async fn job(&self, video_id: &str) -> bool {
        let shutdown = &self.inner.deps.shutdown;
        // Biased: a job that starts during shutdown must not spend an attempt.
        let permit = tokio::select! {
            biased;
            () = shutdown.cancelled() => return false,
            permit = self.inner.slot.acquire() => permit,
        };
        let Ok(_permit) = permit else {
            return false;
        };
        let dir = self.inner.deps.work_dir.join(video_id);
        let outcome = tokio::select! {
            biased;
            () = shutdown.cancelled() => Err(JobError::Interrupted),
            outcome = self.run(video_id, &dir) => outcome,
        };
        let mut settled = !matches!(outcome, Err(JobError::Interrupted | JobError::Store(_)));
        // The audio goes before the row reports the job over, so a finished
        // transcript never has audio left on disk.
        remove_dir(&dir).await;
        let failure = match outcome {
            Ok(false) => None,
            Ok(true) => {
                let now = self.now();
                let finished = self
                    .update(video_id, move |row| {
                        row.state = TranscriptState::Ready;
                        row.finished_at = Some(now);
                        row.last_read_at = now;
                    })
                    .await;
                match finished {
                    Ok(Some(row)) => {
                        tracing::info!(
                            target: LOG,
                            "Transcribed {video_id}: {} of audio, {} segments",
                            text::timestamp(row.transcribed_ms),
                            row.segments.len()
                        );
                        None
                    }
                    Ok(None) => None,
                    Err(error) => {
                        settled = false;
                        Some(("internal", error.to_string()))
                    }
                }
            }
            Err(JobError::Interrupted) => {
                tracing::info!(
                    target: LOG,
                    "Transcript of {video_id} interrupted by shutdown; it resumes at the next start"
                );
                None
            }
            Err(JobError::Failed { code, message }) => Some((code, message)),
            Err(JobError::Store(error)) => Some(("internal", error.to_string())),
        };
        if let Some((code, message)) = failure {
            tracing::warn!(target: LOG, "Transcript of {video_id} failed ({code}): {message}");
            let now = self.now();
            let stored = self
                .update(video_id, move |row| {
                    row.state = TranscriptState::Failed;
                    row.finished_at = Some(now);
                    row.last_read_at = now;
                    row.error_code = Some(code.to_owned());
                    row.error = Some(message);
                })
                .await;
            if let Err(error) = stored {
                settled = false;
                tracing::warn!(
                    target: LOG,
                    "Could not record the failed transcript of {video_id}: {error}"
                );
            }
        }
        self.bump();
        settled
    }

    /// Downloads and transcribes; `true` once every chunk is stored. `false`
    /// when the row was deleted under the job.
    async fn run(&self, video_id: &str, dir: &Path) -> Result<bool, JobError> {
        let media = &self.inner.deps.media;
        let now = self.now();
        let Some(row) = self
            .update(video_id, move |row| {
                row.attempts += 1;
                row.state = TranscriptState::Downloading;
                row.started_at.get_or_insert(now);
            })
            .await?
        else {
            return Ok(false);
        };
        if row.attempts > MAX_ATTEMPTS {
            return Err(failed(
                "interrupted",
                format!(
                    "Restarts interrupted this transcript {MAX_ATTEMPTS} times; start it again"
                ),
            ));
        }
        let speech = self.speech().ok_or_else(|| {
            failed(
                "speech_unavailable",
                "The local speech models are not loaded",
            )
        })?;
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|e| failed("internal", format!("create the work directory: {e}")))?;

        let info = media
            .probe(video_id)
            .await
            .map_err(|e| failed("download_failed", e.to_string()))?;
        let details = info.clone();
        self.update(video_id, move |row| {
            row.title = details.title;
            row.channel = details.channel;
            row.duration_ms = details.duration_ms;
            row.upload_date = details.upload_date;
        })
        .await?;
        if matches!(info.live_status.as_deref(), Some("is_live" | "is_upcoming")) {
            return Err(failed(
                "live",
                "The video is a live or upcoming stream; transcribe it once it has ended",
            ));
        }
        let duration_ms = info
            .duration_ms
            .ok_or_else(|| failed("no_duration", "yt-dlp reports no duration for the video"))?;
        if duration_ms > MAX_DURATION_MS {
            return Err(failed(
                "too_long",
                format!(
                    "The video runs {}, over the {} hour limit",
                    text::timestamp(duration_ms),
                    MAX_DURATION_MS / 3_600_000
                ),
            ));
        }
        let file = media.download(video_id, dir).await.map_err(|e| match e {
            MediaError::TooLarge => failed("too_large", e.to_string()),
            other => failed("download_failed", other.to_string()),
        })?;

        let model = speech.model();
        self.update(video_id, move |row| {
            row.state = TranscriptState::Transcribing;
            row.model = Some(model.to_owned());
        })
        .await?;
        let chunk_samples =
            usize::try_from(CHUNK_MS).unwrap_or(0) / 1000 * SPEECH_SAMPLE_RATE as usize;
        let mut offset_ms = row.transcribed_ms.max(0);
        loop {
            let samples = match media.decode(&file, offset_ms, CHUNK_MS).await {
                Ok(samples) => samples,
                // Some containers make ffmpeg fail, not return nothing, on a
                // seek to the very end.
                Err(_) if offset_ms > 0 && offset_ms + 2000 >= duration_ms => Vec::new(),
                Err(error) => return Err(failed("decode_failed", error.to_string())),
            };
            if samples.is_empty() {
                break;
            }
            let len = samples.len();
            // A window short by over a second is the end of the file; a
            // few samples short is only resampler rounding.
            let last = len + SPEECH_SAMPLE_RATE as usize <= chunk_samples;
            let chunk = speech
                .transcribe_chunk(Arc::new(samples), last)
                .await
                .map_err(|e| failed("speech_failed", e.to_string()))?;
            let consumed = match chunk.consumed_samples {
                0 => len,
                n => n.min(len),
            };
            let advance_ms = samples_to_ms(consumed);
            let base_ms = offset_ms;
            let segments: Vec<TranscriptSegmentData> = chunk
                .segments
                .into_iter()
                .filter(|segment| segment.start_sample < consumed)
                .map(|segment| TranscriptSegmentData {
                    start_ms: base_ms + samples_to_ms(segment.start_sample),
                    end_ms: base_ms + samples_to_ms(segment.end_sample),
                    text: segment.text,
                    undetected: !segment.detected,
                })
                .collect();
            let gaps: Vec<TranscriptGapData> = chunk
                .gaps
                .iter()
                .filter(|gap| gap.start_sample < consumed)
                .map(|gap| TranscriptGapData {
                    start_ms: base_ms + samples_to_ms(gap.start_sample),
                    end_ms: base_ms + samples_to_ms(gap.end_sample),
                })
                .collect();
            let speech_ms = samples_to_ms(chunk.speech_samples);
            offset_ms += advance_ms;
            let transcribed_ms = offset_ms;
            let stored = self
                .update(video_id, move |row| {
                    row.segments.extend(segments);
                    row.gaps.extend(gaps);
                    // A transcript begun before coverage was recorded stays unknown.
                    row.speech_ms = row.speech_ms.map(|ms| ms + speech_ms);
                    row.transcribed_ms = transcribed_ms;
                })
                .await?;
            if stored.is_none() {
                return Ok(false);
            }
            if last || advance_ms == 0 {
                break;
            }
            if offset_ms > MAX_DURATION_MS + CHUNK_MS {
                return Err(failed("too_long", "The audio runs past the length limit"));
            }
        }
        Ok(true)
    }

    /// Empties the work directory. Called once at boot, before the server
    /// accepts requests and so before any job: what is there belonged to a
    /// process that no longer runs.
    pub async fn clear_work_dir(&self) {
        // A preview never ran a job, and must not delete anything.
        if self.inner.deps.mode != SideEffectMode::Live {
            return;
        }
        let dir = &self.inner.deps.work_dir;
        remove_dir(dir).await;
        if let Err(error) = tokio::fs::create_dir_all(dir).await {
            tracing::warn!(target: LOG, "Could not create {}: {error}", dir.display());
        }
    }

    /// Restarts the jobs a shutdown interrupted; returns how many.
    pub async fn resume_interrupted(&self) -> Result<usize, TranscriptError> {
        let rows = self
            .inner
            .deps
            .store
            .read(|tx| tx.get_all::<YoutubeTranscriptData>())
            .await?;
        let mut active: Vec<&YoutubeTranscriptData> =
            rows.iter().filter(|row| row.state.is_active()).collect();
        active.sort_by_key(|row| row.requested_at);
        for row in &active {
            self.ensure_running(&row.video_id);
        }
        Ok(active.len())
    }

    /// Deletes transcripts not read for [`RETENTION_MS`], then the least
    /// recently read beyond [`MAX_STORED`], and any work directory without a
    /// job. Running and queued jobs are never touched.
    pub async fn cleanup(&self) -> Result<CleanupReport, TranscriptError> {
        let now = self.now();
        let (expired, over_limit) = self
            .inner
            .deps
            .store
            .write(move |tx| {
                let mut finished: Vec<YoutubeTranscriptData> = tx
                    .get_all::<YoutubeTranscriptData>()?
                    .into_iter()
                    .filter(|row| !row.state.is_active())
                    .collect();
                finished.sort_by_key(|row| row.last_read_at);
                let expired = finished
                    .iter()
                    .filter(|row| row.last_read_at + RETENTION_MS <= now)
                    .count();
                let over_limit = (finished.len() - expired).saturating_sub(MAX_STORED);
                // Oldest first: the expired rows, then the overflow.
                for row in finished.iter().take(expired + over_limit) {
                    tx.delete::<YoutubeTranscriptData>(&row.video_id)?;
                }
                Ok::<_, StoreError>((expired, over_limit))
            })
            .await?;
        let stale_directories = self.remove_stale_directories().await;
        if expired + over_limit > 0 {
            self.bump();
        }
        Ok(CleanupReport {
            expired,
            over_limit,
            stale_directories,
        })
    }

    async fn remove_stale_directories(&self) -> usize {
        let Ok(mut entries) = tokio::fs::read_dir(&self.inner.deps.work_dir).await else {
            return 0;
        };
        let mut removed = 0;
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().into_owned();
            if self.running().contains(&name) {
                continue;
            }
            remove_dir(&entry.path()).await;
            removed += 1;
        }
        removed
    }
}

/// Removes a directory tree (or a stray file); a missing path is fine.
async fn remove_dir(path: &Path) {
    let result = match tokio::fs::remove_dir_all(path).await {
        Err(error) if error.kind() == std::io::ErrorKind::NotADirectory => {
            tokio::fs::remove_file(path).await
        }
        other => other,
    };
    if let Err(error) = result
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(target: LOG, "Could not remove {}: {error}", path.display());
    }
}

/// `YoutubeTranscriptCleanup`: the daily retention pass ([`Transcripts::cleanup`]).
pub struct TranscriptCleanupTask {
    schedule: CronSchedule,
    transcripts: Transcripts,
    last_summary: Mutex<Option<String>>,
}

impl TranscriptCleanupTask {
    pub fn new(schedule: CronSchedule, transcripts: Transcripts) -> Self {
        Self {
            schedule,
            transcripts,
            last_summary: Mutex::new(None),
        }
    }
}

impl Task for TranscriptCleanupTask {
    fn name(&self) -> &str {
        CLEANUP_TASK
    }

    fn schedule(&self) -> &CronSchedule {
        &self.schedule
    }

    fn options(&self) -> TaskOptions {
        TaskOptions {
            jitter: Duration::ZERO,
            run_on_startup: false,
        }
    }

    fn run<'a>(&'a self, _cx: &'a RunContext) -> BoxFuture<'a, Result<(), TaskError>> {
        Box::pin(async move {
            let report = self
                .transcripts
                .cleanup()
                .await
                .map_err(TaskError::from_error)?;
            let summary = format!(
                "deleted {} expired and {} over-limit transcript(s), {} stale work director(ies)",
                report.expired, report.over_limit, report.stale_directories
            );
            tracing::info!(target: LOG, "YouTube transcript cleanup: {summary}");
            *self
                .last_summary
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(summary);
            Ok(())
        })
    }

    fn last_run_summary(&self) -> Option<String> {
        self.last_summary
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}
