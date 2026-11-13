//! Parcel's `add-delivery` limit: 20 requests per day per API key, failed
//! ones included. The email pipeline and `parcels_add` share the key, so both
//! count their requests here. The pipeline is never gated (its volume is a
//! few emails a day); `parcels_add` reserves a slot before each request and
//! stops at [`TOOL_CEILING`], which leaves room for email.

use omni_store::cbor::Extra;
use omni_store::entity::{Entity, EntityOps as _, EntityWrite as _, UpsertOpts};
use omni_store::{Store, StoreError};
use serde::{Deserialize, Serialize};

const HOUR_MS: i64 = 60 * 60 * 1000;
/// Parcel's "per day", read as a rolling 24 hours.
pub const WINDOW_MS: i64 = 24 * HOUR_MS;
/// Parcel's documented daily limit.
pub const PARCEL_DAILY_LIMIT: usize = 20;
/// `parcels_add` refuses once this many requests (both paths) happened in
/// the window, keeping the rest of [`PARCEL_DAILY_LIMIT`] for email.
pub const TOOL_CEILING: usize = 15;
/// After a 429 without `Retry-After`, `parcels_add` pauses this long.
pub const RATE_LIMIT_BACKOFF_MS: i64 = HOUR_MS;
/// The longest pause a `Retry-After` can impose.
pub const MAX_BACKOFF_MS: i64 = WINDOW_MS;

const KEY: &str = "parcel";

/// Request times in the window (entity `parcel-add-ledger`, key `parcel`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddLedger {
    pub key: String,
    /// Oldest first.
    #[serde(default)]
    pub attempts: Vec<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backoff_until: Option<i64>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Entity for AddLedger {
    const NAME: &'static str = "parcel-add-ledger";
    type Key = String;
    fn key(&self) -> String {
        self.key.clone()
    }
}

impl AddLedger {
    fn prune(&mut self, now: i64) {
        self.attempts.retain(|at| now - at < WINDOW_MS);
    }

    /// `Some(until)` when `parcels_add` may not send at `now`.
    pub fn blocked_until(&self, now: i64) -> Option<i64> {
        if let Some(until) = self.backoff_until
            && now < until
        {
            return Some(until);
        }
        let recent: Vec<i64> = self
            .attempts
            .iter()
            .copied()
            .filter(|at| now - at < WINDOW_MS)
            .collect();
        if recent.len() < TOOL_CEILING {
            return None;
        }
        // The slot frees when enough of the oldest attempts leave the window.
        recent
            .get(recent.len() - TOOL_CEILING)
            .map(|at| at + WINDOW_MS)
    }
}

pub(crate) fn load(tx: &impl omni_store::DocOps) -> Result<AddLedger, StoreError> {
    Ok(tx.get::<AddLedger>(&KEY.to_owned())?.unwrap_or(AddLedger {
        key: KEY.to_owned(),
        ..AddLedger::default()
    }))
}

/// Counts one pipeline request (never refuses).
pub async fn note(store: &Store, now: i64) -> Result<(), StoreError> {
    store
        .write(move |tx| {
            let mut ledger = load(tx)?;
            ledger.prune(now);
            ledger.attempts.push(now);
            tx.upsert(&ledger, UpsertOpts::default())
        })
        .await
}

/// Takes a `parcels_add` slot inside the caller's transaction: `Err(until)`
/// when the ceiling or a 429 pause blocks it.
pub(crate) fn take(tx: &mut omni_store::Tx<'_>, now: i64) -> Result<Result<(), i64>, StoreError> {
    let mut ledger = load(tx)?;
    if let Some(until) = ledger.blocked_until(now) {
        return Ok(Err(until));
    }
    ledger.prune(now);
    ledger.attempts.push(now);
    tx.upsert(&ledger, UpsertOpts::default())?;
    Ok(Ok(()))
}

/// Pauses `parcels_add` after a 429; returns when it may send again.
pub async fn back_off(
    store: &Store,
    now: i64,
    retry_after_ms: Option<i64>,
) -> Result<i64, StoreError> {
    let pause = retry_after_ms
        .unwrap_or(RATE_LIMIT_BACKOFF_MS)
        .clamp(RATE_LIMIT_BACKOFF_MS, MAX_BACKOFF_MS);
    let until = now + pause;
    store
        .write(move |tx| {
            let mut ledger = load(tx)?;
            ledger.backoff_until = Some(ledger.backoff_until.unwrap_or(0).max(until));
            tx.upsert(&ledger, UpsertOpts::default())
        })
        .await?;
    Ok(until)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ledger(attempts: Vec<i64>, backoff_until: Option<i64>) -> AddLedger {
        AddLedger {
            key: KEY.to_owned(),
            attempts,
            backoff_until,
            extra: Extra::default(),
        }
    }

    #[test]
    fn allows_requests_below_the_ceiling() {
        let now = 10 * WINDOW_MS;
        let attempts = (0..TOOL_CEILING as i64 - 1).map(|i| now - i).collect();
        assert_eq!(ledger(attempts, None).blocked_until(now), None);
    }

    #[test]
    fn blocks_at_the_ceiling_until_the_oldest_counted_attempt_expires() {
        let now = 10 * WINDOW_MS;
        let mut attempts: Vec<i64> = vec![now - WINDOW_MS - 5];
        attempts.extend((0..TOOL_CEILING as i64).map(|i| now - 1_000 * (TOOL_CEILING as i64 - i)));
        let first_counted = attempts[1];
        assert_eq!(
            ledger(attempts, None).blocked_until(now),
            Some(first_counted + WINDOW_MS)
        );
    }

    #[test]
    fn ignores_attempts_older_than_the_window() {
        let now = 10 * WINDOW_MS;
        let attempts = (0..TOOL_CEILING as i64)
            .map(|i| now - WINDOW_MS - i)
            .collect();
        assert_eq!(ledger(attempts, None).blocked_until(now), None);
    }

    #[test]
    fn blocks_during_a_rate_limit_pause() {
        assert_eq!(ledger(vec![], Some(500)).blocked_until(400), Some(500));
        assert_eq!(ledger(vec![], Some(500)).blocked_until(500), None);
    }
}
