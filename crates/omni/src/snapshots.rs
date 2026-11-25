//! `DbSnapshot`: hourly consistent copies of the docstore for backups
//! ("Database snapshots" in `docs/architecture.md`).
//!
//! Kopia copies `/data` every 20 minutes. Copying a live WAL-mode SQLite file
//! is not guaranteed to restore, so Kopia skips the live files and backs up
//! these snapshots instead. Each snapshot is `VACUUM INTO` from a read-only
//! connection: one read transaction, so the copy is consistent, and in WAL
//! mode it never blocks the server's writes. The copy runs in a child
//! process (`omni db-snapshot`) under `nice -n 19 ionice -c 3` when those
//! exist, so it yields to everything else on Boris.
//!
//! Every snapshot is checked before it is kept: `PRAGMA quick_check` and a
//! row count, and once a day the full restore check: `PRAGMA
//! integrity_check` plus the compat audit, which decodes every row with
//! this build's entity types. A file that fails is deleted and the run fails.
//!
//! Retention: the newest [`KEEP_HOURLY`] snapshots plus the newest one of
//! each of the last [`KEEP_DAILY`] days. Kopia keeps the longer history.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use omni_runtime::{AppContext, HealthCheck, Subsystem};
use omni_tasks::{CronSchedule, RunContext, Task, TaskError, TaskOptions};
use serde::{Deserialize, Serialize};

const LOG: &str = "Main:DbSnapshot";
pub const TASK: &str = "DbSnapshot";
/// Minute 47: clear of `StoreMaintenance` (:17) and the top-of-hour tasks.
pub const SCHEDULE: &str = "47 * * * *";
pub const DIR: &str = "snapshots";
const PREFIX: &str = "docstore-";
const SUFFIX: &str = ".db";
/// Kopia's policy skips `*.tmp`, so a copy in progress is never backed up.
const PARTIAL_SUFFIX: &str = ".db.tmp";
pub const KEEP_HOURLY: usize = 24;
pub const KEEP_DAILY: usize = 7;
/// The local hour after which the day's first snapshot gets the full check.
const FULL_CHECK_HOUR: i8 = 3;
const CHILD_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// The deep-health check reports a snapshot older than this.
const STALE_AFTER_MS: i64 = 2 * 60 * 60 * 1000 + 15 * 60 * 1000;

/// What `omni db-snapshot` prints (one JSON line) and the task records.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotReport {
    pub bytes: u64,
    pub rows: u64,
    pub copy_ms: u64,
    pub check_ms: u64,
    /// `integrity_check` and the compat audit ran (else `quick_check`).
    pub full: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("the snapshot failed its check: {0}")]
    Check(String),
    #[error("snapshot child process: {0}")]
    Child(String),
}

/// Copies `db` into `out` with `VACUUM INTO` and checks the copy. Blocking.
pub fn snapshot_file(db: &Path, out: &Path, full: bool) -> Result<SnapshotReport, SnapshotError> {
    if out.exists() {
        std::fs::remove_file(out)?;
    }
    let started = Instant::now();
    {
        let source = rusqlite::Connection::open_with_flags(
            db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        source.busy_timeout(Duration::from_secs(30))?;
        let target = out.to_string_lossy().into_owned();
        source.execute("VACUUM INTO ?1", [target])?;
    }
    let copy_ms = millis(started.elapsed());
    let checked = Instant::now();
    let rows = match check_file(out, full) {
        Ok(rows) => rows,
        Err(error) => {
            let _ = std::fs::remove_file(out);
            return Err(error);
        }
    };
    let file = std::fs::File::open(out)?;
    file.sync_all()?;
    Ok(SnapshotReport {
        bytes: file.metadata()?.len(),
        rows,
        copy_ms,
        check_ms: millis(checked.elapsed()),
        full,
    })
}

fn millis(elapsed: Duration) -> u64 {
    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
}

/// The restore check: the file opens, passes SQLite's check and, when
/// `full`, every row decodes with this build's entity types.
pub fn check_file(path: &Path, full: bool) -> Result<u64, SnapshotError> {
    let conn = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let pragma = if full {
        "integrity_check"
    } else {
        "quick_check"
    };
    let verdict: String = conn.query_row(&format!("PRAGMA {pragma}"), [], |row| row.get(0))?;
    if verdict != "ok" {
        return Err(SnapshotError::Check(format!("{pragma}: {verdict}")));
    }
    let rows: i64 = conn.query_row("SELECT count(*) FROM blobs", [], |row| row.get(0))?;
    if rows == 0 {
        return Err(SnapshotError::Check("the copy has no rows".to_owned()));
    }
    if full {
        let now = jiff::Timestamp::now().as_millisecond();
        let audit = crate::compat_audit::audit(&conn, &crate::compat_audit::entity_catalog(), now)
            .map_err(|e| SnapshotError::Check(format!("compat audit: {e:#}")))?;
        if audit.blocking() > 0 {
            return Err(SnapshotError::Check(format!(
                "compat audit found {} row(s) this build cannot read",
                audit.blocking()
            )));
        }
    }
    Ok(u64::try_from(rows).unwrap_or(0))
}

/// `docstore-20261124T1847Z.db` for `at`.
fn file_name(at: jiff::Timestamp) -> String {
    format!("{PREFIX}{}{SUFFIX}", at.strftime("%Y%m%dT%H%MZ"))
}

fn parse_name(name: &str) -> Option<jiff::Timestamp> {
    let stamp = name.strip_prefix(PREFIX)?.strip_suffix(SUFFIX)?;
    jiff::Timestamp::strptime("%Y%m%dT%H%MZ%z", format!("{stamp}+0000")).ok()
}

/// Kept snapshots, newest first.
pub fn list(dir: &Path) -> std::io::Result<Vec<(jiff::Timestamp, PathBuf)>> {
    let mut found = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(found),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        if let Some(at) = name.to_str().and_then(parse_name) {
            found.push((at, entry.path()));
        }
    }
    found.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    Ok(found)
}

/// Which of `snapshots` (newest first) retention deletes: everything but
/// the newest `hourly` and the newest of each of the newest `daily` days.
pub fn to_prune(
    snapshots: &[(jiff::Timestamp, PathBuf)],
    tz: &jiff::tz::TimeZone,
    hourly: usize,
    daily: usize,
) -> Vec<PathBuf> {
    let mut days_seen = Vec::new();
    let mut doomed = Vec::new();
    for (index, (at, path)) in snapshots.iter().enumerate() {
        let day = at.to_zoned(tz.clone()).date();
        let first_of_day = !days_seen.contains(&day);
        if first_of_day {
            days_seen.push(day);
        }
        let keep = index < hourly || (first_of_day && days_seen.len() <= daily);
        if !keep {
            doomed.push(path.clone());
        }
    }
    doomed
}

pub struct SnapshotTask {
    schedule: CronSchedule,
    db: PathBuf,
    dir: PathBuf,
    tz: jiff::tz::TimeZone,
    clock: omni_core::clock::SharedClock,
}

impl SnapshotTask {
    /// Whether this run gets the full restore check: the first snapshot
    /// taken after [`FULL_CHECK_HOUR`] local time each day.
    fn full_check_due(&self, now: jiff::Timestamp) -> bool {
        let local = now.to_zoned(self.tz.clone());
        if local.hour() < FULL_CHECK_HOUR {
            return false;
        }
        let today = local.date();
        let Ok(kept) = list(&self.dir) else {
            return true;
        };
        !kept.iter().any(|(at, _)| {
            let zoned = at.to_zoned(self.tz.clone());
            zoned.date() == today && zoned.hour() >= FULL_CHECK_HOUR
        })
    }

    async fn take(&self, out: &Path, full: bool) -> Result<SnapshotReport, SnapshotError> {
        match low_priority_command(&self.db, out, full) {
            Some(mut command) => {
                let child = command
                    .kill_on_drop(true)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .map_err(|e| SnapshotError::Child(e.to_string()))?;
                let output = tokio::time::timeout(CHILD_TIMEOUT, child.wait_with_output())
                    .await
                    .map_err(|_| SnapshotError::Child("timed out".to_owned()))?
                    .map_err(|e| SnapshotError::Child(e.to_string()))?;
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return Err(SnapshotError::Child(
                        stderr.lines().last().unwrap_or("failed").to_owned(),
                    ));
                }
                serde_json::from_slice(&output.stdout)
                    .map_err(|e| SnapshotError::Child(format!("unreadable report: {e}")))
            }
            None => {
                let (db, out) = (self.db.clone(), out.to_path_buf());
                tokio::task::spawn_blocking(move || snapshot_file(&db, &out, full))
                    .await
                    .map_err(|e| SnapshotError::Child(e.to_string()))?
            }
        }
    }

    async fn run_once(&self) -> Result<String, SnapshotError> {
        std::fs::create_dir_all(&self.dir)?;
        let now = jiff::Timestamp::from_millisecond(self.clock.now_ms())
            .unwrap_or_else(|_| jiff::Timestamp::now());
        let full = self.full_check_due(now);
        let name = file_name(now);
        let partial = self
            .dir
            .join(format!("{}{PARTIAL_SUFFIX}", name.trim_end_matches(SUFFIX)));
        let report = self.take(&partial, full).await?;
        let path = self.dir.join(&name);
        std::fs::rename(&partial, &path)?;
        if let Ok(dir) = std::fs::File::open(&self.dir) {
            let _ = dir.sync_all();
        }
        let kept = list(&self.dir)?;
        let mut pruned = 0;
        for doomed in to_prune(&kept, &self.tz, KEEP_HOURLY, KEEP_DAILY) {
            std::fs::remove_file(&doomed)?;
            pruned += 1;
        }
        let summary = format!(
            "{name}: {:.1} MB, {} rows, copied in {} ms, {} in {} ms; pruned {pruned}",
            report.bytes as f64 / 1_048_576.0,
            report.rows,
            report.copy_ms,
            if report.full {
                "integrity check and compat audit"
            } else {
                "quick check"
            },
            report.check_ms,
        );
        tracing::info!(target: LOG, "{summary}");
        Ok(summary)
    }
}

/// `nice -n 19 ionice -c 3 <this binary> db-snapshot ...` when both tools
/// exist (the Docker image), else `None` (run in-process).
fn low_priority_command(db: &Path, out: &Path, full: bool) -> Option<tokio::process::Command> {
    let exe = std::env::current_exe().ok()?;
    let found = |tool: &str| {
        ["/usr/bin", "/bin", "/usr/sbin", "/sbin"]
            .iter()
            .map(|dir| Path::new(dir).join(tool))
            .find(|path| path.is_file())
    };
    let (nice, ionice) = (found("nice")?, found("ionice")?);
    let mut command = tokio::process::Command::new(nice);
    command
        .args(["-n", "19"])
        .arg(ionice)
        .args(["-c", "3"])
        .arg(exe)
        .arg("db-snapshot")
        .arg("--db")
        .arg(db)
        .arg("--out")
        .arg(out);
    if full {
        command.arg("--full");
    }
    Some(command)
}

impl Task for SnapshotTask {
    fn name(&self) -> &str {
        TASK
    }

    fn display_name(&self) -> Option<&str> {
        Some("Database snapshot")
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
            match self.run_once().await {
                Ok(_) => Ok(()),
                Err(error) => {
                    tracing::error!(target: LOG, %error, "Database snapshot failed");
                    Err(TaskError::from_error(error))
                }
            }
        })
    }
}

/// The age of the newest snapshot, for `/api/health/deep`.
fn age_check(dir: PathBuf, clock: omni_core::clock::SharedClock) -> HealthCheck {
    HealthCheck::new("db_snapshot", false, move || {
        let dir = dir.clone();
        let now = clock.now_ms();
        async move {
            let newest = list(&dir).map_err(|e| e.to_string())?.into_iter().next();
            let Some((at, _)) = newest else {
                return Err("no snapshot yet".to_owned());
            };
            let age = now - at.as_millisecond();
            let minutes = age / 60_000;
            if age > STALE_AFTER_MS {
                Err(format!("newest snapshot is {minutes} minutes old"))
            } else {
                Ok(format!("newest snapshot is {minutes} minutes old"))
            }
        }
    })
}

/// The `DbSnapshot` task and its health check.
pub fn subsystem(ctx: &AppContext) -> Result<Subsystem, omni_tasks::InvalidScheduleError> {
    let tz = jiff::tz::TimeZone::get(&ctx.config.tz).unwrap_or(jiff::tz::TimeZone::UTC);
    let dir = ctx.paths.data_dir.join(DIR);
    let task = SnapshotTask {
        schedule: CronSchedule::parse(SCHEDULE, &tz)?,
        db: ctx.config.db_path(),
        dir: dir.clone(),
        tz,
        clock: ctx.clock.clone(),
    };
    let mut subsystem = Subsystem::named("snapshots");
    subsystem.tasks.push(Arc::new(task));
    subsystem
        .health_checks
        .push(age_check(dir, ctx.clock.clone()));
    Ok(subsystem)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> jiff::Timestamp {
        text.parse().unwrap()
    }

    #[test]
    fn names_round_trip() {
        let stamp = at("2026-11-24T18:47:00Z");
        assert_eq!(file_name(stamp), "docstore-20261124T1847Z.db");
        assert_eq!(parse_name(&file_name(stamp)), Some(stamp));
        assert_eq!(parse_name("docstore-20261124T1847Z.db.tmp"), None);
        assert_eq!(parse_name("docstore.db"), None);
    }

    #[test]
    fn retention_keeps_recent_hours_and_one_per_day() {
        let tz = jiff::tz::TimeZone::UTC;
        // Hourly for ten days, newest first.
        let newest = at("2026-11-24T23:47:00Z");
        let snapshots: Vec<(jiff::Timestamp, PathBuf)> = (0..240)
            .map(|h| {
                let stamp = newest - jiff::SignedDuration::from_hours(h);
                (stamp, PathBuf::from(file_name(stamp)))
            })
            .collect();
        let doomed = to_prune(&snapshots, &tz, 24, 7);
        let kept: Vec<_> = snapshots
            .iter()
            .filter(|(_, p)| !doomed.contains(p))
            .map(|(at, _)| *at)
            .collect();
        // 24 hourly (all of the newest day) plus the newest of six older days.
        assert_eq!(kept.len(), 30);
        assert!(kept.contains(&at("2026-11-23T23:47:00Z")));
        assert!(kept.contains(&at("2026-11-18T23:47:00Z")));
        assert!(!kept.contains(&at("2026-11-17T23:47:00Z")));
        assert!(!kept.contains(&at("2026-11-23T22:47:00Z")));
    }

    #[test]
    fn a_snapshot_is_a_checked_consistent_copy() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("docstore.db");
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch(
                "PRAGMA journal_mode=WAL;
                 CREATE TABLE blobs (pk TEXT PRIMARY KEY, entity TEXT, version INTEGER, expires_at INTEGER, updated_at INTEGER, data BLOB);",
            )
            .unwrap();
            // Rows only in the WAL still reach the copy.
            conn.execute(
                "INSERT INTO blobs (pk, entity, version, data) VALUES ('$x#s1:a', NULL, 0, NULL)",
                [],
            )
            .unwrap();
            let out = dir.path().join("snap.db.tmp");
            let report = snapshot_file(&db, &out, false).unwrap();
            assert_eq!(report.rows, 1);
            assert!(report.bytes > 0);
            assert_eq!(check_file(&out, false).unwrap(), 1);
        }
        let corrupt = dir.path().join("corrupt.db");
        std::fs::write(&corrupt, b"not a database").unwrap();
        assert!(check_file(&corrupt, false).is_err());
    }
}
