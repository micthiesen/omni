//! Run history entities in their exact stored shapes, and their store operations.

use omni_store::cbor::{self, Extra};
use omni_store::entity::{self, Entity, EntityOps as _, EntityWrite as _, ModifyOpts, UpsertOpts};
use omni_store::{DocOps as _, DocWrite as _, LogLine, Store, StoreError, logs_gz};
use serde::{Deserialize, Serialize};

use crate::Trigger;

/// Logger name used for persistence diagnostics.
const LOG: &str = "Main:TaskRegistry";

/// Runs kept per task; older runs (and their logs) are pruned at run start.
pub const KEEP_PER_TASK: usize = 50;

/// `error` recorded for runs a previous process left `running`.
pub const INTERRUPTED_ERROR: &str = "interrupted (process exited)";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskRunStatus {
    Running,
    Success,
    Error,
    /// Completed without an error but skipped its real work because an
    /// upstream failed; `error` holds the reason. Rows written before this
    /// variant existed never contain it, so they decode as before.
    Degraded,
}

/// `task-run`, keyed by `runId` (`"<task>:<uuidv4>"`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskRunData {
    pub run_id: String,
    pub task_name: String,
    pub trigger: Trigger,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduled_for: Option<i64>,
    pub started_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<i64>,
    pub status: TaskRunStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Entity for TaskRunData {
    const NAME: &'static str = "task-run";
    type Key = String;
    fn key(&self) -> String {
        self.run_id.clone()
    }
}

/// `task-schedule-state`, keyed by `taskName`: the catch-up cursor.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskScheduleState {
    pub task_name: String,
    pub schedule: String,
    pub evaluated_through: i64,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Entity for TaskScheduleState {
    const NAME: &'static str = "task-schedule-state";
    type Key = String;
    fn key(&self) -> String {
        self.task_name.clone()
    }
}

/// `task-run-log`, keyed by `runId`: one row per finished run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskRunLog {
    pub run_id: String,
    pub task_name: String,
    /// Legacy rows written before compression.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<Vec<LogLine>>,
    /// `omni_store::logs_gz` encoding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines_gz: Option<String>,
    /// Oldest lines dropped once the per-run cap was hit.
    pub dropped: u64,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Entity for TaskRunLog {
    const NAME: &'static str = "task-run-log";
    type Key = String;
    fn key(&self) -> String {
        self.run_id.clone()
    }
}

/// Decoded run logs (`TaskRunLogData`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunLogsData {
    pub run_id: String,
    pub task_name: String,
    pub lines: Vec<LogLine>,
    pub dropped: u64,
}

pub fn make_run_id(task_name: &str) -> String {
    format!("{task_name}:{}", omni_core::ids::uuid_v4())
}

/// The run row and cursor written when a run starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunStart {
    pub task_name: String,
    pub trigger: Trigger,
    /// The task's cron expression, stored with the cursor.
    pub schedule: String,
    /// New catch-up cursor (`scheduledFor ?? startedAt`).
    pub evaluated_through: i64,
    pub run_id: String,
    pub scheduled_for: Option<i64>,
    pub started_at: i64,
}

/// Upserts a `running` row and prunes the task's history,
/// without touching the catch-up cursor.
pub async fn record_run_start(
    store: &Store,
    task_name: &str,
    trigger: Trigger,
    run_id: Option<String>,
    scheduled_for: Option<i64>,
    started_at: i64,
) -> Result<TaskRunData, StoreError> {
    let run = new_run(
        run_id.unwrap_or_else(|| make_run_id(task_name)),
        task_name,
        trigger,
        scheduled_for,
        started_at,
    );
    let task_name = task_name.to_owned();
    let stored = run.clone();
    store
        .write(move |tx| {
            tx.upsert(&stored, UpsertOpts::default())?;
            prune_in(tx, &task_name)
        })
        .await?;
    Ok(run)
}

fn new_run(
    run_id: String,
    task_name: &str,
    trigger: Trigger,
    scheduled_for: Option<i64>,
    started_at: i64,
) -> TaskRunData {
    TaskRunData {
        run_id,
        task_name: task_name.to_owned(),
        trigger,
        scheduled_for,
        started_at,
        finished_at: None,
        status: TaskRunStatus::Running,
        error: None,
        summary: None,
        extra: Extra::new(),
    }
}

/// The run row, the catch-up cursor and the
/// 50-run prune in one transaction, so a crash can never advance the cursor
/// without a corresponding run row.
pub async fn record_run_start_and_mark_schedule(
    store: &Store,
    start: RunStart,
) -> Result<TaskRunData, StoreError> {
    let run = new_run(
        start.run_id,
        &start.task_name,
        start.trigger,
        start.scheduled_for,
        start.started_at,
    );
    let state = TaskScheduleState {
        task_name: start.task_name.clone(),
        schedule: start.schedule,
        evaluated_through: start.evaluated_through,
        extra: Extra::new(),
    };
    let stored = run.clone();
    let task_name = start.task_name;
    store
        .write(move |tx| {
            tx.upsert(&stored, UpsertOpts::default())?;
            tx.upsert(&state, UpsertOpts::default())?;
            prune_in(tx, &task_name)
        })
        .await?;
    Ok(run)
}

/// Deletes the runs (and their log rows) beyond the newest
/// [`KEEP_PER_TASK`] for `task_name`. Reads raw rows including expired ones
/// and skips undecodable rows.
fn prune_in(tx: &mut omni_store::Tx<'_>, task_name: &str) -> Result<(), StoreError> {
    let prefix = entity::prefix::<TaskRunData>(&[])?;
    let mut runs = Vec::new();
    for row in tx.get_raw_rows_by_prefix(&prefix)? {
        let Some(data) = row.data else { continue };
        let Ok(value) = cbor::decode(&data) else {
            continue;
        };
        if let Ok(run) = cbor::from_value::<TaskRunData>(value) {
            runs.push(run);
        }
    }
    let stale: Vec<String> = select_runs_to_prune(&runs, task_name, KEEP_PER_TASK)
        .into_iter()
        .map(|run| run.run_id.clone())
        .collect();
    for run_id in stale {
        tx.delete_doc(&entity::pk::<TaskRunData>(&run_id)?)?;
        tx.delete_doc(&entity::pk::<TaskRunLog>(&run_id)?)?;
    }
    Ok(())
}

/// The settled outcome written by [`record_run_end`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunEnd {
    pub status: TaskRunStatus,
    pub error: Option<String>,
    pub summary: Option<String>,
    pub finished_at: i64,
}

/// Patches status, error, summary and `finishedAt`. Returns
/// the settled run, or `None` when the row no longer exists.
pub async fn record_run_end(
    store: &Store,
    run_id: &str,
    end: RunEnd,
) -> Result<Option<TaskRunData>, StoreError> {
    let run_id = run_id.to_owned();
    store
        .write(move |tx| {
            tx.update::<TaskRunData>(
                &run_id,
                |mut run| {
                    run.status = end.status;
                    run.error = end.error.map(omni_core::secrets::scrub_owned);
                    run.summary = end.summary;
                    run.finished_at = Some(end.finished_at);
                    run
                },
                ModifyOpts::default(),
            )
        })
        .await
}

/// Flips runs left `running` by a crashed process to
/// errors. Returns how many were repaired.
pub async fn mark_interrupted_runs(store: &Store, now: i64) -> Result<usize, StoreError> {
    store
        .write(move |tx| {
            let interrupted: Vec<String> = tx
                .get_all::<TaskRunData>()?
                .into_iter()
                .filter(|run| run.status == TaskRunStatus::Running)
                .map(|run| run.run_id)
                .collect();
            for run_id in &interrupted {
                tx.update::<TaskRunData>(
                    run_id,
                    |mut run| {
                        run.status = TaskRunStatus::Error;
                        run.error = Some(INTERRUPTED_ERROR.to_owned());
                        run.finished_at = Some(now);
                        run
                    },
                    ModifyOpts::default(),
                )?;
            }
            Ok(interrupted.len())
        })
        .await
}

/// Newest first, optionally for one task.
pub async fn get_runs(
    store: &Store,
    task_name: Option<&str>,
    limit: usize,
) -> Result<Vec<TaskRunData>, StoreError> {
    let task_name = task_name.map(str::to_owned);
    let mut runs = store
        .read(move |docs| {
            Ok(docs
                .get_all::<TaskRunData>()?
                .into_iter()
                .filter(|run| {
                    task_name
                        .as_deref()
                        .is_none_or(|name| run.task_name == name)
                })
                .collect::<Vec<_>>())
        })
        .await?;
    runs.sort_by_key(|run| std::cmp::Reverse(run.started_at));
    runs.truncate(limit);
    Ok(runs)
}

pub async fn get_last_run(
    store: &Store,
    task_name: &str,
) -> Result<Option<TaskRunData>, StoreError> {
    Ok(get_runs(store, Some(task_name), 1)
        .await?
        .into_iter()
        .next())
}

pub async fn get_run(store: &Store, run_id: &str) -> Result<Option<TaskRunData>, StoreError> {
    let run_id = run_id.to_owned();
    store
        .read(move |docs| docs.get::<TaskRunData>(&run_id))
        .await
}

pub async fn get_task_schedule_state(
    store: &Store,
    task_name: &str,
) -> Result<Option<TaskScheduleState>, StoreError> {
    let task_name = task_name.to_owned();
    store
        .read(move |docs| docs.get::<TaskScheduleState>(&task_name))
        .await
}

pub async fn mark_schedule_evaluated(
    store: &Store,
    task_name: &str,
    schedule: &str,
    evaluated_through: i64,
) -> Result<(), StoreError> {
    let state = TaskScheduleState {
        task_name: task_name.to_owned(),
        schedule: schedule.to_owned(),
        evaluated_through,
        extra: Extra::new(),
    };
    store
        .write(move |tx| tx.upsert(&state, UpsertOpts::default()))
        .await
}

/// One compressed row per run; nothing for a silent run.
pub async fn save_run_logs(store: &Store, data: RunLogsData) -> Result<(), StoreError> {
    if data.lines.is_empty() && data.dropped == 0 {
        return Ok(());
    }
    let row = TaskRunLog {
        run_id: data.run_id,
        task_name: data.task_name,
        lines: None,
        lines_gz: Some(logs_gz::encode(&data.lines)?),
        dropped: data.dropped,
        extra: Extra::new(),
    };
    store
        .write(move |tx| tx.upsert(&row, UpsertOpts::default()))
        .await
}

/// Reads compressed or legacy rows. An unreadable row (for
/// example one half-written when the container was killed) is deleted with a
/// warning and treated as no logs, so the endpoint cannot fail forever.
pub async fn get_run_logs(store: &Store, run_id: &str) -> Result<Option<RunLogsData>, StoreError> {
    let key = run_id.to_owned();
    let read = store.read(move |docs| docs.get::<TaskRunLog>(&key)).await;
    let decoded = match read {
        Ok(None) => return Ok(None),
        Ok(Some(row)) => match &row.lines_gz {
            Some(gz) => logs_gz::decode(gz).map(|lines| (row, lines)),
            None => {
                let lines = row.lines.clone().unwrap_or_default();
                Ok((row, lines))
            }
        },
        Err(error) => Err(error),
    };
    match decoded {
        Ok((row, lines)) => Ok(Some(RunLogsData {
            run_id: row.run_id,
            task_name: row.task_name,
            lines,
            dropped: row.dropped,
        })),
        Err(error @ (StoreError::Sqlite(_) | StoreError::Closed)) => Err(error),
        Err(error) => {
            let key = run_id.to_owned();
            store.write(move |tx| tx.delete::<TaskRunLog>(&key)).await?;
            tracing::warn!(
                target: LOG,
                "Dropped unreadable log row for run \"{run_id}\": {error}"
            );
            Ok(None)
        }
    }
}

/// The runs beyond the newest `keep` for `task_name`.
pub fn select_runs_to_prune<'a>(
    runs: &'a [TaskRunData],
    task_name: &str,
    keep: usize,
) -> Vec<&'a TaskRunData> {
    let mut matching: Vec<&TaskRunData> =
        runs.iter().filter(|r| r.task_name == task_name).collect();
    matching.sort_by_key(|run| std::cmp::Reverse(run.started_at));
    matching.into_iter().skip(keep).collect()
}

impl From<&TaskRunData> for omni_api::runs::Run {
    fn from(run: &TaskRunData) -> Self {
        use omni_api::runs::{RunStatus, RunTrigger};
        omni_api::runs::Run {
            run_id: run.run_id.clone(),
            task_name: run.task_name.clone(),
            trigger: match run.trigger {
                Trigger::Schedule => RunTrigger::Schedule,
                Trigger::Manual => RunTrigger::Manual,
                Trigger::Startup => RunTrigger::Startup,
                Trigger::Catchup => RunTrigger::Catchup,
            },
            scheduled_for: run.scheduled_for,
            started_at: run.started_at,
            finished_at: run.finished_at,
            status: match run.status {
                TaskRunStatus::Running => RunStatus::Running,
                TaskRunStatus::Success => RunStatus::Success,
                TaskRunStatus::Error => RunStatus::Error,
                TaskRunStatus::Degraded => RunStatus::Degraded,
            },
            error: run.error.clone(),
            summary: run.summary.clone(),
        }
    }
}

#[cfg(test)]
mod persistence_spec {
    use super::*;

    fn make_run(task: &str, started_at: i64) -> TaskRunData {
        new_run(
            format!("{task}:{started_at}"),
            task,
            Trigger::Schedule,
            None,
            started_at,
        )
    }

    #[test]
    fn keeps_the_newest_n_runs_for_the_task() {
        let runs: Vec<_> = (1..=5).map(|i| make_run("A", i * 1000)).collect();
        let stale = select_runs_to_prune(&runs, "A", 3);
        assert_eq!(
            stale.iter().map(|r| r.started_at).collect::<Vec<_>>(),
            vec![2000, 1000]
        );
    }

    #[test]
    fn returns_nothing_at_or_under_the_keep_limit() {
        let runs: Vec<_> = (1..=3).map(|i| make_run("A", i * 1000)).collect();
        assert!(select_runs_to_prune(&runs, "A", 3).is_empty());
    }

    #[test]
    fn only_considers_runs_for_the_given_task() {
        let runs: Vec<_> = (1..=3)
            .map(|i| make_run("A", i * 1000))
            .chain((1..=3).map(|i| make_run("B", i * 1000)))
            .collect();
        let stale = select_runs_to_prune(&runs, "A", 2);
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].task_name, "A");
    }
}
