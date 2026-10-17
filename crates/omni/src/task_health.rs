//! `TaskHealth`: after every settled run, applies the persistent-failure rule
//! ([`omni_tasks::health`]) to the task's durable history and sends one
//! Pushover note per incident plus one recovery note. It reacts to the
//! registry's `RunFinished` events and judges from run history alone, so a
//! missed event or a restart is corrected by the task's next run.

use omni_alerts::{PushOutcome, PushoverChannel, PushoverError, PushoverMessage};
use omni_runtime::{AppContext, BackgroundService, RetryPolicy};
use omni_store::entity::{EntityOps as _, EntityWrite as _, UpsertOpts};
use omni_store::{Store, StoreError};
use omni_tasks::TaskRunEventKind;
use omni_tasks::health::{
    IncidentDecision, PERSISTENT_FAILURE, TaskHealthIncident, decide_incident,
};
use omni_tasks::persistence::{self, KEEP_PER_TASK};
use tokio::sync::broadcast::error::RecvError;

const LOG: &str = "Main:TaskHealth";
pub const NAME: &str = "TaskHealth";

/// The message to send after one evaluation, if any.
#[derive(Clone, Debug, PartialEq)]
pub enum HealthNote {
    Unhealthy(TaskHealthIncident),
    Recovered(TaskHealthIncident),
}

/// Applies the rule to `task_name` and settles its incident row. A new
/// incident is reserved durably before it is returned, so a note is never
/// sent twice; [`release`] undoes the reservation when delivery is definitely
/// rejected or skipped.
pub async fn evaluate(
    store: &Store,
    task_name: &str,
    now: i64,
) -> Result<Option<HealthNote>, StoreError> {
    let runs = persistence::get_runs(store, Some(task_name), KEEP_PER_TASK).await?;
    let name = task_name.to_owned();
    store
        .write(move |tx| {
            let incident = tx.get::<TaskHealthIncident>(&name)?;
            match decide_incident(&PERSISTENT_FAILURE, &runs, incident.as_ref(), now) {
                IncidentDecision::Nothing => Ok(None),
                IncidentDecision::Notify(incident) => {
                    tx.upsert(&incident, UpsertOpts::default())?;
                    Ok(Some(HealthNote::Unhealthy(incident)))
                }
                IncidentDecision::Recover(incident) => {
                    tx.delete::<TaskHealthIncident>(&incident.task_name)?;
                    Ok(Some(HealthNote::Recovered(incident)))
                }
            }
        })
        .await
}

/// Drops an undelivered incident reservation so the next run retries it.
pub async fn release(store: &Store, incident: &TaskHealthIncident) -> Result<(), StoreError> {
    let name = incident.task_name.clone();
    let notified_at = incident.notified_at;
    store
        .write(move |tx| {
            let current = tx.get::<TaskHealthIncident>(&name)?;
            if current.is_some_and(|c| c.notified_at == notified_at) {
                tx.delete::<TaskHealthIncident>(&name)?;
            }
            Ok(())
        })
        .await
}

pub fn message(note: &HealthNote, tz: &jiff::tz::TimeZone) -> PushoverMessage {
    match note {
        HealthNote::Unhealthy(incident) => {
            let since = jiff::Timestamp::from_millisecond(incident.first_bad_at)
                .map(|t| t.to_zoned(tz.clone()).strftime("%a %-I:%M %p").to_string())
                .unwrap_or_default();
            let latest = incident
                .reason
                .as_deref()
                .map(|r| format!("\nLatest: {r}"))
                .unwrap_or_default();
            PushoverMessage {
                title: Some(format!("{} is unhealthy", incident.task_name)),
                message: format!(
                    "{} consecutive failed or degraded runs since {since}.{latest}",
                    incident.bad_runs
                ),
                ..PushoverMessage::default()
            }
        }
        HealthNote::Recovered(incident) => PushoverMessage {
            title: Some(format!("{} recovered", incident.task_name)),
            message: "The latest run succeeded.".to_owned(),
            ..PushoverMessage::default()
        },
    }
}

/// Evaluates `task_name` and delivers the resulting note, if any.
pub async fn check(ctx: &AppContext, tz: &jiff::tz::TimeZone, task_name: &str) {
    let note = match evaluate(&ctx.store, task_name, ctx.clock.now_ms()).await {
        Ok(Some(note)) => note,
        Ok(None) => return,
        Err(error) => {
            tracing::warn!(target: LOG, %error, "Task health check for \"{task_name}\" failed");
            return;
        }
    };
    let sent = ctx
        .pushover
        .send(PushoverChannel::General, message(&note, tz))
        .await;
    let release_reservation = !delivered_or_uncertain(&sent);
    match (&note, sent) {
        (_, Ok(PushOutcome::Sent | PushOutcome::Recorded)) => {
            tracing::info!(target: LOG, "Sent task health note for \"{task_name}\": {note:?}");
        }
        (_, Ok(outcome)) => {
            tracing::debug!(target: LOG, "Task health note for \"{task_name}\" not sent ({outcome:?})");
        }
        (HealthNote::Unhealthy(_), Err(error)) if release_reservation => {
            tracing::warn!(target: LOG, %error, "Task health alert for \"{task_name}\" was rejected; retrying after the next run");
        }
        (HealthNote::Unhealthy(_), Err(error)) => {
            tracing::warn!(target: LOG, %error, "Task health alert for \"{task_name}\" may not have been delivered; not resending");
        }
        (HealthNote::Recovered(_), Err(error)) => {
            tracing::warn!(target: LOG, %error, "Task health recovery note for \"{task_name}\" failed");
        }
    }
    if let HealthNote::Unhealthy(incident) = &note
        && release_reservation
        && let Err(error) = release(&ctx.store, incident).await
    {
        tracing::warn!(target: LOG, %error, "Release task health incident failed");
    }
}

/// Whether an incident note may have reached the user, so its reservation is
/// kept: a delivery, or an outcome that is unknown (timeout, socket error,
/// 5xx). A definite 4xx rejection or an unsent message releases it so the
/// next run retries; an uncertain one is never resent.
pub fn delivered_or_uncertain(sent: &Result<PushOutcome, PushoverError>) -> bool {
    match sent {
        Ok(PushOutcome::Sent | PushOutcome::Recorded) => true,
        Ok(PushOutcome::SkippedNoToken | PushOutcome::Disabled) => false,
        Err(error) => !error.is_definite_rejection(),
    }
}

/// The hidden service: evaluates each task as its runs settle. A lagged
/// receiver re-evaluates every registered task.
pub fn service() -> BackgroundService {
    BackgroundService {
        name: NAME,
        start: Box::new(|ctx: AppContext| {
            Box::pin(async move {
                let tz = jiff::tz::TimeZone::get(&ctx.config.tz).unwrap_or(jiff::tz::TimeZone::UTC);
                let mut events = ctx.bus.task_runs();
                loop {
                    let event = tokio::select! {
                        () = ctx.shutdown.cancelled() => return,
                        event = events.recv() => event,
                    };
                    match event {
                        Ok(event) if event.kind == TaskRunEventKind::RunFinished => {
                            check(&ctx, &tz, &event.task_name).await;
                        }
                        Ok(_) => {}
                        Err(RecvError::Lagged(_)) => {
                            for name in ctx.tasks.names() {
                                check(&ctx, &tz, &name).await;
                            }
                        }
                        Err(RecvError::Closed) => return,
                    }
                }
            })
        }),
        retry: Some(RetryPolicy {
            initial: std::time::Duration::from_secs(30),
            max: std::time::Duration::from_secs(300),
        }),
    }
}
