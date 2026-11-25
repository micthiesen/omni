//! `Retention`: daily cleanup of operational records that nothing else
//! bounds ("Retention" in `docs/architecture.md`). Only Omni's own records
//! and scratch files: never recipes, reminders, mail, media, contacts or the
//! stylist ledger.
//!
//! | What | Kept |
//! |---|---|
//! | MCP call traces (`mcp-call`) | 30 days (and the newest 10,000) |
//! | Email archive receipts (`email-archive-*`) | 180 days |
//! | Email draft and send receipts (`email-compose-*`) | 180 days |
//! | Parcel submission dedup (`parcel-submitted-delivery`) | 365 days |
//! | Rows of the retired shell tools (`shell-run`) | none |
//! | Per-run Markdown logs under `LOGS_PATH/<feature>/` | 90 days |
//! | Daily log files `LOGS_PATH/omni-*.log` | 14 days |
//! | Orphaned scratch files in the temp directory (`pp_*`, `omni-*`) | 1 day |
//!
//! Bounded elsewhere: task runs (newest 50 per task), the event journal
//! (7 days, failed 30), email activity (1,000 per pipeline), receipts and
//! subscriptions, YouTube transcripts, archives and DB snapshots. Kept on
//! purpose: AI cost events (the all-time totals read them), recommendation
//! attempts and taste evidence (they keep a title from being offered twice),
//! and the calendar's tracked events (their tombstones keep a deleted event
//! deleted).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use futures::future::BoxFuture;
use omni_runtime::{AppContext, Subsystem};
use omni_store::{DocWrite as _, Store, StoreError};
use omni_tasks::{CronSchedule, RunContext, Task, TaskError, TaskOptions};

const LOG: &str = "Main:Retention";
pub const TASK: &str = "Retention";
/// 04:37 local: after the nightly backups, clear of the other 4 a.m. tasks.
pub const SCHEDULE: &str = "37 4 * * *";
const DAY_MS: i64 = 24 * 60 * 60 * 1000;
const RUN_LOG_DAYS: u64 = 90;
const TEMP_DAYS: u64 = 1;

/// `(entity, days kept)` for operational docstore rows.
const ENTITY_RETENTION: [(&str, i64); 6] = [
    ("email-archive-action", 180),
    ("email-archive-message", 180),
    ("email-archive-history", 180),
    ("email-compose-draft", 180),
    ("email-compose-send", 180),
    ("parcel-submitted-delivery", 365),
];

/// Entities of removed features: their rows have no reader.
const RETIRED_ENTITIES: [&str; 1] = ["shell-run"];

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub calls: usize,
    pub rows: u64,
    pub retired: u64,
    pub log_files: usize,
    pub temp: usize,
}

impl Report {
    pub fn summary(&self) -> String {
        format!(
            "removed {} MCP call trace(s), {} old operational row(s), {} retired row(s), {} log file(s), {} scratch file(s)",
            self.calls, self.rows, self.retired, self.log_files, self.temp
        )
    }
}

pub struct RetentionTask {
    schedule: CronSchedule,
    store: Store,
    clock: omni_core::clock::SharedClock,
    logs: Option<PathBuf>,
    temp: PathBuf,
}

/// Deletes files under `dir` (recursively when `deep`) whose name passes
/// `matches` and that were last modified more than `days` ago; how many.
pub fn sweep_files(dir: &Path, days: u64, deep: bool, matches: &dyn Fn(&str) -> bool) -> usize {
    let Some(cutoff) = SystemTime::now().checked_sub(Duration::from_secs(days * 24 * 60 * 60))
    else {
        return 0;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        if meta.is_dir() {
            if deep {
                removed += sweep_files(&path, days, deep, matches);
            }
            continue;
        }
        let old = meta.modified().is_ok_and(|at| at < cutoff);
        if old && matches(&name) && std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Scratch directories a killed process left behind (`omni-*`, `pp_*`).
fn sweep_temp(dir: &Path) -> usize {
    let Some(cutoff) = SystemTime::now().checked_sub(Duration::from_secs(TEMP_DAYS * 86_400))
    else {
        return 0;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !(name.starts_with("pp_") || name.starts_with("omni-")) {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.modified().is_ok_and(|at| at < cutoff) {
            continue;
        }
        let gone = if meta.is_dir() {
            std::fs::remove_dir_all(entry.path())
        } else {
            std::fs::remove_file(entry.path())
        };
        if gone.is_ok() {
            removed += 1;
        }
    }
    removed
}

impl RetentionTask {
    pub async fn run_once(&self) -> Result<Report, StoreError> {
        let now = self.clock.now_ms();
        let calls = omni_mcp::activity::prune_old_calls(&self.store, now).await?;
        let (rows, retired) = self
            .store
            .write(move |tx| {
                let mut rows = 0;
                for (entity, days) in ENTITY_RETENTION {
                    rows += tx.delete_docs_by_entity_before(entity, now - days * DAY_MS)?;
                }
                let mut retired = 0;
                for entity in RETIRED_ENTITIES {
                    retired += tx.delete_docs_by_entity(entity)?;
                }
                Ok::<_, StoreError>((rows, retired))
            })
            .await?;
        let (logs, temp) = (self.logs.clone(), self.temp.clone());
        let (log_files, temp) = tokio::task::spawn_blocking(move || {
            let mut removed = 0;
            if let Some(logs) = &logs {
                if let Ok(entries) = std::fs::read_dir(logs) {
                    for entry in entries.flatten() {
                        if entry.metadata().is_ok_and(|m| m.is_dir()) {
                            removed += sweep_files(&entry.path(), RUN_LOG_DAYS, true, &|_| true);
                        }
                    }
                }
                let before = count_files(logs);
                crate::logging::sweep_old(logs, now, crate::logging::FILE_RETAIN_DAYS);
                removed += before.saturating_sub(count_files(logs));
            }
            (removed, sweep_temp(&temp))
        })
        .await
        .unwrap_or_default();
        Ok(Report {
            calls,
            rows,
            retired,
            log_files,
            temp,
        })
    }
}

fn count_files(dir: &Path) -> usize {
    std::fs::read_dir(dir).map_or(0, |entries| {
        entries
            .flatten()
            .filter(|e| e.metadata().is_ok_and(|m| m.is_file()))
            .count()
    })
}

impl Task for RetentionTask {
    fn name(&self) -> &str {
        TASK
    }

    fn display_name(&self) -> Option<&str> {
        Some("Retention cleanup")
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
            let report = self.run_once().await.map_err(TaskError::from_error)?;
            tracing::info!(target: LOG, "{}", report.summary());
            Ok(())
        })
    }
}

/// The `Retention` task.
pub fn subsystem(ctx: &AppContext) -> Result<Subsystem, omni_tasks::InvalidScheduleError> {
    let tz = jiff::tz::TimeZone::get(&ctx.config.tz).unwrap_or(jiff::tz::TimeZone::UTC);
    let task = RetentionTask {
        schedule: CronSchedule::parse(SCHEDULE, &tz)?,
        store: ctx.store.clone(),
        clock: ctx.clock.clone(),
        logs: ctx.config.logs_path.as_ref().map(PathBuf::from),
        temp: std::env::temp_dir(),
    };
    let mut subsystem = Subsystem::named("retention");
    subsystem.tasks.push(Arc::new(task));
    Ok(subsystem)
}

#[cfg(test)]
mod tests {
    use super::*;
    use omni_store::cbor::JsValue;
    use omni_store::{DocMeta, DocOps as _};

    #[tokio::test]
    async fn removes_only_old_operational_rows_and_retired_entities() {
        let clock = omni_testkit::test_clock(400 * DAY_MS);
        let store = omni_testkit::TestStore::new(clock.clone()).await;
        let meta = |entity: &str, at: i64| DocMeta {
            entity: Some(entity.to_owned()),
            version: 0,
            expires_at: None,
            updated_at: Some(at),
        };
        store
            .store
            .write(move |tx| {
                let now = 400 * DAY_MS;
                tx.upsert_doc(
                    "$a1",
                    &JsValue::Null,
                    meta("email-compose-send", now - 200 * DAY_MS),
                )?;
                tx.upsert_doc(
                    "$a2",
                    &JsValue::Null,
                    meta("email-compose-send", now - 10 * DAY_MS),
                )?;
                tx.upsert_doc(
                    "$p1",
                    &JsValue::Null,
                    meta("parcel-submitted-delivery", now - 200 * DAY_MS),
                )?;
                tx.upsert_doc("$s1", &JsValue::Null, meta("shell-run", now))?;
                // User content is never touched, however old.
                tx.upsert_doc("$r1", &JsValue::Null, meta("recipes-whatever", 0))?;
                Ok::<_, StoreError>(())
            })
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let task = RetentionTask {
            schedule: CronSchedule::parse(SCHEDULE, &jiff::tz::TimeZone::UTC).unwrap(),
            store: store.store.clone(),
            clock,
            logs: Some(dir.path().to_path_buf()),
            temp: dir.path().join("tmp"),
        };
        let report = task.run_once().await.unwrap();
        assert_eq!(report.rows, 1);
        assert_eq!(report.retired, 1);
        let left = store
            .store
            .read(|docs| docs.get_raw_rows_by_prefix("$"))
            .await
            .unwrap();
        let keys: Vec<&str> = left.iter().map(|r| r.pk.as_str()).collect();
        assert_eq!(keys, ["$a2", "$p1", "$r1"]);
    }

    #[test]
    fn old_run_logs_and_scratch_files_are_swept() {
        let dir = tempfile::tempdir().unwrap();
        let feature = dir.path().join("parcel-tracker");
        std::fs::create_dir_all(&feature).unwrap();
        let old = feature.join("2026-01-01.md");
        let new = feature.join("2026-11-24.md");
        std::fs::write(&old, "x").unwrap();
        std::fs::write(&new, "x").unwrap();
        let long_ago = SystemTime::now() - Duration::from_secs(100 * 86_400);
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(long_ago)
            .unwrap();
        assert_eq!(sweep_files(&feature, 90, true, &|_| true), 1);
        assert!(!old.exists() && new.exists());

        let scratch = dir.path().join("pp_chunk");
        std::fs::write(&scratch, "x").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&scratch)
            .unwrap()
            .set_modified(long_ago)
            .unwrap();
        let unrelated = dir.path().join("keep-me");
        std::fs::write(&unrelated, "x").unwrap();
        assert_eq!(sweep_temp(dir.path()), 1);
        assert!(!scratch.exists() && unrelated.exists());
    }
}
