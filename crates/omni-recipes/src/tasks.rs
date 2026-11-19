//! Scheduled work: the hourly archive, the daily purge, and the timer
//! service that closes idle cook sessions and announces ended timers.

use std::collections::HashSet;
use std::time::Duration;

use futures::future::BoxFuture;
use omni_tasks::{CronSchedule, InvalidScheduleError, RunContext, Task, TaskError, TaskOptions};
use rusqlite::{Connection, params};
use serde_json::Value;

use crate::archive::BackupOutcome;
use crate::db::next_seq;
use crate::error::{R, io, sql};
use crate::meta;
use crate::organize::RETENTION_MS;
use crate::service::Recipes;

const LOG: &str = "Recipes";

pub const BACKUP_TASK: &str = "RecipesBackup";
/// Hourly at 37 minutes past.
pub const BACKUP_SCHEDULE: &str = "37 * * * *";
pub const PURGE_TASK: &str = "RecipesPurge";
/// Daily at 04:12 local time.
pub const PURGE_SCHEDULE: &str = "12 4 * * *";
/// How often the timer service looks for idle sessions and ended timers.
pub const TICK_EVERY: Duration = Duration::from_secs(5);
/// An upload that never completed is dropped after a day.
const STALE_UPLOAD_MS: i64 = 24 * 60 * 60 * 1000;
/// Image files newer than this are never purged: their row may not have
/// committed yet.
pub const FILE_GRACE: Duration = Duration::from_secs(24 * 60 * 60);

/// What one purge removed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Purged {
    pub recipes: u32,
    pub changes: u32,
    pub inbox_items: u32,
    pub files: u32,
}

fn purge_tx(conn: &Connection, now: i64) -> R<Purged> {
    let cutoff = now - RETENTION_MS;
    let mut purged = Purged::default();
    let mut statement = conn
        .prepare(
            "SELECT id, merged_into FROM recipes_recipe
             WHERE state = 'trashed' AND state_changed_at <= ?1 ORDER BY id",
        )
        .map_err(sql)?;
    let trashed: Vec<(String, Option<String>)> = statement
        .query_map(params![cutoff], |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(sql)?
        .collect::<Result<_, _>>()
        .map_err(sql)?;
    for (id, merged_into) in &trashed {
        // An absorbed recipe's id keeps resolving after it is gone.
        match merged_into {
            Some(target) => {
                conn.execute(
                    "INSERT OR REPLACE INTO recipes_redirect (id, merged_into, purged_at)
                     VALUES (?1, ?2, ?3)",
                    params![id, target, now],
                )
                .map_err(sql)?;
                conn.execute(
                    "UPDATE recipes_redirect SET merged_into = ?2 WHERE merged_into = ?1",
                    params![id, target],
                )
                .map_err(sql)?;
            }
            None => {
                conn.execute(
                    "DELETE FROM recipes_redirect WHERE merged_into = ?1",
                    params![id],
                )
                .map_err(sql)?;
            }
        }
        // Revisions go first: their trigger needs the recipe still in the trash.
        for statement in [
            "DELETE FROM recipes_revision WHERE recipe_id = ?1",
            "DELETE FROM recipes_recipe_tag WHERE recipe_id = ?1",
            "DELETE FROM recipes_image WHERE recipe_id = ?1",
            "DELETE FROM recipes_cook_event WHERE session_id IN
               (SELECT id FROM recipes_cook WHERE recipe_id = ?1)",
            "DELETE FROM recipes_timer_alert WHERE session_id IN
               (SELECT id FROM recipes_cook WHERE recipe_id = ?1)",
            "DELETE FROM recipes_cook WHERE recipe_id = ?1",
            "DELETE FROM recipes_fts WHERE recipe_id = ?1",
            "DELETE FROM recipes_recipe WHERE id = ?1",
            // The recipe's stored source goes with it, unless another recipe
            // (a second import, or the recipe that absorbed this one) uses it.
            "DELETE FROM recipes_inbox_file WHERE item_id IN
               (SELECT id FROM recipes_inbox WHERE recipe_id = ?1 AND state = 'imported'
                  AND id NOT IN (SELECT source_item FROM recipes_recipe WHERE source_item IS NOT NULL))",
            "DELETE FROM recipes_inbox WHERE recipe_id = ?1 AND state = 'imported'
               AND id NOT IN (SELECT source_item FROM recipes_recipe WHERE source_item IS NOT NULL)",
        ] {
            conn.execute(statement, params![id]).map_err(sql)?;
        }
        purged.recipes += 1;
    }
    let count = |changed: usize| u32::try_from(changed).unwrap_or(u32::MAX);
    purged.changes = count(
        conn.execute("DELETE FROM recipes_change WHERE at <= ?1", params![cutoff])
            .map_err(sql)?,
    );
    conn.execute(
        "DELETE FROM recipes_client_op WHERE applied_at <= ?1",
        params![cutoff],
    )
    .map_err(sql)?;
    conn.execute(
        "DELETE FROM recipes_timer_alert WHERE at <= ?1",
        params![cutoff],
    )
    .map_err(sql)?;
    // Items an agent set aside, and uploads that never completed.
    conn.execute(
        "DELETE FROM recipes_inbox_file WHERE item_id IN
           (SELECT id FROM recipes_inbox
            WHERE (state IN ('discarded', 'duplicate') AND resolved_at <= ?1)
               OR (state = 'uploading' AND created_at <= ?2))",
        params![cutoff, now - STALE_UPLOAD_MS],
    )
    .map_err(sql)?;
    purged.inbox_items = count(
        conn.execute(
            "DELETE FROM recipes_inbox
             WHERE (state IN ('discarded', 'duplicate') AND resolved_at <= ?1)
                OR (state = 'uploading' AND created_at <= ?2)",
            params![cutoff, now - STALE_UPLOAD_MS],
        )
        .map_err(sql)?,
    );
    if purged.recipes + purged.changes + purged.inbox_items > 0 {
        next_seq(conn)?;
    }
    Ok(purged)
}

/// Every digest something still refers to: rows, stored lead images, and the
/// change log's snapshots, so an undo within 30 days finds its file.
fn kept_digests(conn: &Connection) -> R<HashSet<String>> {
    let mut kept: HashSet<String> = crate::archive::referenced_digests(conn)?
        .into_iter()
        .collect();
    let mut statement = conn
        .prepare("SELECT before, after FROM recipes_change")
        .map_err(sql)?;
    let mut rows = statement.query([]).map_err(sql)?;
    while let Some(row) = rows.next().map_err(sql)? {
        for index in 0..2 {
            let text: String = row.get(index).map_err(sql)?;
            if let Ok(snapshot) = serde_json::from_str::<Value>(&text) {
                meta::digests(&snapshot, &mut kept);
            }
        }
    }
    Ok(kept)
}

impl Recipes {
    /// Empties the trash of recipes deleted 30 days ago (keeping the pointer
    /// of a merged-away recipe), drops change-log entries older than 30
    /// days, and deletes image files nothing refers to any more.
    pub async fn purge(&self) -> R<Purged> {
        // Nothing is removed for good before a verified archive holds it.
        let due = self
            .transact(false, |conn, now| {
                conn.query_row(
                    "SELECT count(*) FROM recipes_recipe
                     WHERE state = 'trashed' AND state_changed_at <= ?1",
                    params![now - RETENTION_MS],
                    |row| row.get::<_, u32>(0),
                )
                .map_err(sql)
            })
            .await?;
        if due > 0 {
            let archive = self.archive_before_purge().await?;
            tracing::info!(
                target: LOG,
                "Wrote {archive} before removing {due} recipes from the trash for good"
            );
        }
        let mut purged = self.transact(false, purge_tx).await?;
        let held = self.files_lock.lock().await;
        let kept = self.query(kept_digests).await?;
        let files = self.files.clone();
        let grace = self.file_grace;
        purged.files = tokio::task::spawn_blocking(move || -> std::io::Result<u32> {
            let mut removed = 0;
            for digest in files.digests()? {
                if kept.contains(&digest) {
                    continue;
                }
                let fresh = files
                    .path(&digest, crate::files::Rendition::Original)
                    .and_then(|path| std::fs::metadata(path).ok())
                    .and_then(|file| file.modified().ok())
                    .and_then(|modified| modified.elapsed().ok())
                    .is_none_or(|age| age < grace);
                if !fresh {
                    files.remove(&digest)?;
                    removed += 1;
                }
            }
            // Renditions left without their original, after the same grace.
            for path in files.orphan_renditions()? {
                let old = std::fs::metadata(&path)
                    .and_then(|file| file.modified())
                    .ok()
                    .and_then(|modified| modified.elapsed().ok())
                    .is_some_and(|age| age >= grace);
                if old {
                    std::fs::remove_file(&path)?;
                }
            }
            Ok(removed)
        })
        .await
        .map_err(io)?
        .map_err(io)?;
        drop(held);
        Ok(purged)
    }

    /// The timer service: runs [`Recipes::tick`] every few seconds until
    /// shutdown. A failed pass is logged and the next one runs as usual.
    pub async fn run_timers(&self, shutdown: tokio_util::sync::CancellationToken) {
        let mut ticker = tokio::time::interval(TICK_EVERY);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = shutdown.cancelled() => return,
                _ = ticker.tick() => {}
            }
            if let Err(error) = self.tick().await {
                tracing::warn!(target: LOG, "Cook timer pass failed: {}", error.tool_text());
            }
        }
    }
}

pub struct BackupTask {
    recipes: Recipes,
    schedule: CronSchedule,
}

impl BackupTask {
    pub fn new(recipes: Recipes, tz: &jiff::tz::TimeZone) -> Result<Self, InvalidScheduleError> {
        Ok(Self {
            recipes,
            schedule: CronSchedule::parse(BACKUP_SCHEDULE, tz)?,
        })
    }
}

impl Task for BackupTask {
    fn name(&self) -> &str {
        BACKUP_TASK
    }

    fn display_name(&self) -> Option<&str> {
        Some("Recipes backup")
    }

    fn schedule(&self) -> &CronSchedule {
        &self.schedule
    }

    fn options(&self) -> TaskOptions {
        TaskOptions::default()
    }

    fn run<'a>(&'a self, _cx: &'a RunContext) -> BoxFuture<'a, Result<(), TaskError>> {
        Box::pin(async move {
            match self.recipes.backup().await {
                Ok(BackupOutcome::Empty) => {
                    tracing::info!(target: LOG, "No recipes yet; nothing to back up");
                    Ok(())
                }
                Ok(BackupOutcome::UpToDate { archive_id }) => {
                    tracing::info!(target: LOG, "Recipe archive {archive_id} is current");
                    Ok(())
                }
                Ok(BackupOutcome::Written { archive_id, seq }) => {
                    tracing::info!(target: LOG, "Wrote recipe archive {archive_id} at sequence {seq}");
                    Ok(())
                }
                Err(error) => Err(TaskError::new(error.tool_text())),
            }
        })
    }
}

pub struct PurgeTask {
    recipes: Recipes,
    schedule: CronSchedule,
}

impl PurgeTask {
    pub fn new(recipes: Recipes, tz: &jiff::tz::TimeZone) -> Result<Self, InvalidScheduleError> {
        Ok(Self {
            recipes,
            schedule: CronSchedule::parse(PURGE_SCHEDULE, tz)?,
        })
    }
}

impl Task for PurgeTask {
    fn name(&self) -> &str {
        PURGE_TASK
    }

    fn display_name(&self) -> Option<&str> {
        Some("Recipes purge")
    }

    fn schedule(&self) -> &CronSchedule {
        &self.schedule
    }

    fn options(&self) -> TaskOptions {
        TaskOptions::default()
    }

    fn run<'a>(&'a self, _cx: &'a RunContext) -> BoxFuture<'a, Result<(), TaskError>> {
        Box::pin(async move {
            match self.recipes.purge().await {
                Ok(purged) => {
                    tracing::info!(
                        target: LOG,
                        "Purged {} recipes, {} change-log entries, {} inbox items and {} image files",
                        purged.recipes,
                        purged.changes,
                        purged.inbox_items,
                        purged.files
                    );
                    Ok(())
                }
                Err(error) => Err(TaskError::new(error.tool_text())),
            }
        })
    }
}
