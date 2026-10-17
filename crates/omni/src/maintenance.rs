//! `StoreMaintenance`: hourly at minute 17,
//! deletes up to 1000 expired docstore rows. Reads already hide expired rows,
//! so this changes no visible state. It is a hidden service, not a registry task: it
//! never appears in `/api/tasks` or run history.

use omni_runtime::{AppContext, BackgroundService};
use omni_store::{DocWrite as _, Store, StoreError};
use omni_tasks::CronSchedule;

const LOG: &str = "Main:StoreMaintenance";
pub const NAME: &str = "StoreMaintenance";
pub const SCHEDULE: &str = "0 17 * * * *";
/// Rows removed per pass.
pub const CLEANUP_LIMIT: u32 = 1000;

/// One pass: deletes up to [`CLEANUP_LIMIT`] expired rows.
pub async fn run_once(store: &Store) -> Result<u64, StoreError> {
    let removed = store.write(|tx| tx.cleanup_expired(CLEANUP_LIMIT)).await?;
    if removed > 0 {
        tracing::info!(target: LOG, "Removed {removed} expired row(s)");
    } else {
        tracing::debug!(target: LOG, "No expired rows");
    }
    Ok(removed)
}

/// The hidden service: one pass at every cron fire until shutdown.
pub fn service(schedule: CronSchedule) -> BackgroundService {
    BackgroundService {
        name: NAME,
        start: Box::new(move |ctx: AppContext| {
            let schedule = schedule.clone();
            Box::pin(async move {
                loop {
                    let now = ctx.clock.now();
                    let Some(next) = schedule.next_after(now) else {
                        return;
                    };
                    let wait = next.as_millisecond().saturating_sub(now.as_millisecond());
                    let wait = std::time::Duration::from_millis(u64::try_from(wait).unwrap_or(0));
                    tokio::select! {
                        () = ctx.shutdown.cancelled() => return,
                        () = tokio::time::sleep(wait) => {}
                    }
                    if let Err(error) = run_once(&ctx.store).await {
                        tracing::warn!(target: LOG, error = %error, "Expired row cleanup failed");
                    }
                }
            })
        }),
        retry: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use omni_store::cbor::JsValue;
    use omni_store::{DocMeta, DocOps as _};

    #[tokio::test]
    async fn removes_only_expired_rows() {
        let clock = omni_testkit::test_clock(10_000);
        let store = omni_testkit::TestStore::new(clock.clone()).await;
        store
            .store
            .write(|tx| {
                let meta = |expires_at| DocMeta {
                    entity: Some("x".to_owned()),
                    version: 0,
                    expires_at,
                    updated_at: None,
                };
                tx.upsert_doc("$x#s1:a", &JsValue::Null, meta(Some(5_000)))?;
                tx.upsert_doc("$x#s1:b", &JsValue::Null, meta(Some(50_000)))?;
                tx.upsert_doc("$x#s1:c", &JsValue::Null, meta(None))
            })
            .await
            .unwrap();
        assert_eq!(run_once(&store.store).await.unwrap(), 1);
        let rows = store
            .store
            .read(|docs| docs.get_raw_rows_by_prefix("$x#"))
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn schedule_parses() {
        assert!(CronSchedule::parse(SCHEDULE, &jiff::tz::TimeZone::UTC).is_ok());
    }
}
