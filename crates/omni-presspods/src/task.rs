//! The `PressPods` task: drains the durable job
//! queue. Submissions kick a manual run immediately; the five-minute sweep
//! picks up backoff retries and jobs orphaned by a crash.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::future::BoxFuture;
use omni_http::SideEffectMode;
use omni_tasks::{CronSchedule, RunContext, Task, TaskError, TaskOptions};

use crate::error::PressPodsError;
use crate::model::{JobStatus, PressPodsJob};
use crate::persistence::{FailureDetail, MAX_JOB_ATTEMPTS, job_normalized_url, select_due_jobs};
use crate::service::{PressPods, TASK_NAME};
use crate::storage::checkpoint_work_id;

const LOG: &str = "PressPods";
pub const SCHEDULE: &str = "0 */5 * * * *";

/// How one job ended this pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobOutcome {
    Processed,
    Requeued,
    Failed,
}

/// The queue worker.
pub struct PressPodsTask {
    service: PressPods,
    schedule: CronSchedule,
    reclaimed_on_boot: AtomicBool,
    last_run_summary: Mutex<Option<String>>,
}

impl PressPodsTask {
    pub fn new(service: PressPods, schedule: CronSchedule) -> Self {
        Self {
            service,
            schedule,
            reclaimed_on_boot: AtomicBool::new(false),
            last_run_summary: Mutex::new(None),
        }
    }

    /// One pass: reclaim boot orphans once, then drain until nothing is due
    /// (jobs submitted mid-run are picked up by the same run).
    pub async fn drain(&self, run_id: Option<&str>) -> Result<String, PressPodsError> {
        let service = &self.service;
        service.audio().ensure_dir().await?;
        if service.deps.mode == SideEffectMode::Record {
            // A record-mode (shadow) process never spends model/TTS money or
            // publishes episodes; the live process owns the queue.
            return Ok("Record mode: PressPods jobs are left for the live service".to_owned());
        }
        if !self.reclaimed_on_boot.swap(true, Ordering::SeqCst) {
            let reclaimed = service
                .persistence()
                .reclaim_processing_jobs_at_boot()
                .await?;
            if reclaimed > 0 {
                tracing::warn!(target: LOG, "Reclaiming {reclaimed} job(s) orphaned by a restart");
            }
        }
        let (mut processed, mut requeued, mut failed) = (0u32, 0u32, 0u32);
        // One failed preflight covers the rest of the pass: every other due
        // job is deferred without another probe.
        let mut speech_outage: Option<String> = None;
        loop {
            let now = service.deps.clock.now_ms();
            let jobs = service.persistence().get_all_jobs().await?;
            let Some(job) = select_due_jobs(jobs, now).into_iter().next() else {
                break;
            };
            match self.process_job(job, run_id, &mut speech_outage).await? {
                JobOutcome::Processed => processed += 1,
                JobOutcome::Requeued => requeued += 1,
                JobOutcome::Failed => failed += 1,
            }
        }
        let summary = if processed + requeued + failed == 0 {
            "No episode jobs due".to_owned()
        } else {
            format!("{processed} episode(s) created, {requeued} requeued, {failed} failed")
        };
        if processed + requeued + failed > 0 {
            tracing::info!(target: LOG, "PressPods pass: {summary}");
        }
        Ok(summary)
    }

    async fn process_job(
        &self,
        job: PressPodsJob,
        run_id: Option<&str>,
        speech_outage: &mut Option<String>,
    ) -> Result<JobOutcome, PressPodsError> {
        let service = &self.service;
        let persistence = service.persistence();
        // A stale `processing` claim means a previous run died mid-job: finish
        // the bookkeeping if its episode landed, else count a crashed attempt
        // so a job that kills the process every time still converges.
        if job.status == JobStatus::Processing {
            if let Some(existing) = persistence.find_episode_for_job(&job).await? {
                tracing::info!(
                    target: LOG,
                    "Job for {} already produced episode {}; completing",
                    job.url,
                    existing.episode_id
                );
                let normalized = job_normalized_url(&job);
                crate::events::publish(&service.ports, crate::events::published(&existing, &job))
                    .await;
                service
                    .replace_older_episodes(&normalized, &existing.episode_id)
                    .await?;
                service
                    .audio()
                    .clear_chunk_checkpoints(&checkpoint_work_id(&normalized))
                    .await;
                persistence
                    .complete_job(&job, &existing.episode_id, Vec::new())
                    .await?;
                return Ok(JobOutcome::Processed);
            }
            let updated = persistence
                .record_job_failure(&job, "Process crashed or restarted mid-run", true)
                .await?;
            if updated.status == JobStatus::Queued {
                tracing::warn!(
                    target: LOG,
                    "Reclaimed crashed job for {}; will retry (attempt {}/{MAX_JOB_ATTEMPTS})",
                    job.url,
                    updated.attempts
                );
                return Ok(JobOutcome::Requeued);
            }
            tracing::error!(target: LOG, "Giving up on {}: crashed {} times mid-run", job.url, updated.attempts);
            crate::events::publish(&service.ports, crate::events::failed(&updated)).await;
            return Ok(JobOutcome::Failed);
        }

        // Before the claim and before anything is paid for: with the speech
        // server unreachable the job waits instead of spending retrieval,
        // model calls and an attempt on an episode that cannot be voiced.
        if speech_outage.is_none()
            && let Err(error) = service.deps.tts.preflight().await
        {
            if error.is_speech_unreachable() {
                *speech_outage = Some(error.summary());
            } else {
                tracing::warn!(target: LOG, %error, "Speech preflight could not run; continuing");
            }
        }
        if let Some(outage) = speech_outage.as_deref() {
            return self.defer_for_speech_outage(&job, outage, run_id).await;
        }

        persistence
            .claim_job(&job.job_id, run_id.map(str::to_owned))
            .await?;
        if job.attempts > 0 {
            tracing::info!(
                target: LOG,
                url = job.url.as_str(),
                last_error = job.last_error.as_deref().unwrap_or(""),
                "Retrying episode creation (attempt {}/{MAX_JOB_ATTEMPTS})",
                job.attempts + 1
            );
        } else {
            tracing::info!(target: LOG, "Creating episode for {}", job.url);
        }

        let (result, retrievers) = service
            .create_episode_traced(&job.url, run_id.map(str::to_owned))
            .await;
        match result {
            Ok(episode) => {
                // Before completing: a crash in between recovers this episode
                // from the job and replays the same event key.
                crate::events::publish(&service.ports, crate::events::published(&episode, &job))
                    .await;
                persistence
                    .complete_job(&job, &episode.episode_id, retrievers)
                    .await?;
                Ok(JobOutcome::Processed)
            }
            Err(error) => {
                let retryable = error.is_retryable();
                let summary = error.summary();
                let detail = FailureDetail {
                    operation: Some(error.operation().to_owned()),
                    retrievers,
                };
                let updated = persistence
                    .record_job_failure_with(&job, &summary, retryable, detail)
                    .await?;
                if updated.status == JobStatus::Queued {
                    tracing::warn!(
                        target: LOG,
                        url = job.url.as_str(),
                        error = summary.as_str(),
                        "Episode creation failed, will retry (attempt {}/{MAX_JOB_ATTEMPTS})",
                        updated.attempts
                    );
                    omni_tasks::report_degraded(format!(
                        "episode creation failed, will retry: {summary}"
                    ));
                    Ok(JobOutcome::Requeued)
                } else {
                    tracing::error!(target: LOG, "Episode creation failed permanently for {} {summary}", job.url);
                    crate::events::publish(&service.ports, crate::events::failed(&updated)).await;
                    Ok(JobOutcome::Failed)
                }
            }
        }
    }

    async fn defer_for_speech_outage(
        &self,
        job: &PressPodsJob,
        outage: &str,
        run_id: Option<&str>,
    ) -> Result<JobOutcome, PressPodsError> {
        let service = &self.service;
        let deadline_ms =
            i64::from(service.deps.config.presspods_tts_unreachable_deadline_hours) * 3_600_000;
        let Some(updated) = service
            .persistence()
            .record_speech_unreachable(&job.job_id, outage, run_id.map(str::to_owned), deadline_ms)
            .await?
        else {
            // Deleted or changed since selection; the next pass sees its real state.
            return Ok(JobOutcome::Requeued);
        };
        let summary = updated.last_error.as_deref().unwrap_or(outage);
        if updated.status == JobStatus::Queued {
            tracing::warn!(
                target: LOG,
                url = job.url.as_str(),
                error = summary,
                "Episode creation deferred, no attempt used (check {} of this outage)",
                updated.speech_unreachable_checks.unwrap_or(1)
            );
            omni_tasks::report_degraded(format!("episode creation deferred: {summary}"));
            Ok(JobOutcome::Requeued)
        } else {
            tracing::error!(target: LOG, "Episode creation failed permanently for {} {summary}", job.url);
            crate::events::publish(&service.ports, crate::events::failed(&updated)).await;
            Ok(JobOutcome::Failed)
        }
    }
}

impl Task for PressPodsTask {
    fn name(&self) -> &str {
        TASK_NAME
    }

    fn schedule(&self) -> &CronSchedule {
        &self.schedule
    }

    fn options(&self) -> TaskOptions {
        // At boot so a restart drains queued work and recovers orphans at once.
        TaskOptions {
            jitter: std::time::Duration::ZERO,
            run_on_startup: true,
        }
    }

    fn run<'a>(&'a self, cx: &'a RunContext) -> BoxFuture<'a, Result<(), TaskError>> {
        Box::pin(async move {
            let summary = self.drain(Some(&cx.run_id)).await.map_err(|e| {
                // An unexpected failure fails the run.
                TaskError::new(e.to_string())
            })?;
            if let Ok(mut last) = self.last_run_summary.lock() {
                *last = Some(summary);
            }
            Ok(())
        })
    }

    fn last_run_summary(&self) -> Option<String> {
        self.last_run_summary.lock().ok().and_then(|s| s.clone())
    }
}
