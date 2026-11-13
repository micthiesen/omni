//! Durable delivery dedup gate: every
//! submission is reserved (`pending`, attempt counted) before Parcel is called
//! and confirmed (`submitted` or `rejected`) afterwards, so an interrupted
//! request is replayable and terminal outcomes are never resubmitted.

use omni_store::cbor::Extra;
use omni_store::entity::{Entity, EntityOps as _, EntityWrite as _, UpsertOpts};
use omni_store::{Store, StoreError};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SubmissionStatus {
    Pending,
    Submitted,
    Rejected,
}

/// Who submitted a delivery; absent on email submissions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubmissionOrigin {
    /// The `parcels_add` MCP tool (`emailId` is empty).
    ParcelsAdd,
}

/// A submitted delivery (entity `parcel-submitted-delivery`, key `trackingNumber`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmittedDelivery {
    pub tracking_number: String,
    pub carrier_code: String,
    pub description: String,
    pub submitted_at: i64,
    pub email_id: String,
    /// Missing on legacy rows, which are already terminal submissions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<SubmissionStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempts: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<SubmissionOrigin>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Entity for SubmittedDelivery {
    const NAME: &'static str = "parcel-submitted-delivery";
    type Key = String;
    fn key(&self) -> String {
        self.tracking_number.clone()
    }
}

impl SubmittedDelivery {
    /// Terminal (submitted, rejected or legacy) rows block resubmission. So
    /// does a `parcels_add` reservation: it is in flight or its outcome was
    /// lost, and an uncertain add is never resent, not even by a later email.
    pub fn is_terminal(&self) -> bool {
        self.status != Some(SubmissionStatus::Pending)
            || self.origin == Some(SubmissionOrigin::ParcelsAdd)
    }
}

/// The fields of a reservation or confirmation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeliveryAttempt {
    pub tracking_number: String,
    pub carrier_code: String,
    pub description: String,
    pub submitted_at: i64,
    pub email_id: String,
}

pub async fn get(
    store: &Store,
    tracking_number: &str,
) -> Result<Option<SubmittedDelivery>, StoreError> {
    let key = tracking_number.to_owned();
    store
        .read(move |docs| docs.get::<SubmittedDelivery>(&key))
        .await
}

/// A terminal row exists.
pub async fn has_submitted(store: &Store, tracking_number: &str) -> Result<bool, StoreError> {
    Ok(get(store, tracking_number)
        .await?
        .is_some_and(|row| row.is_terminal()))
}

/// Every terminal tracking number, in store order
/// (the order `findNearDuplicateTracking` reports its first match from).
pub async fn all_tracking_numbers(store: &Store) -> Result<Vec<String>, StoreError> {
    Ok(store
        .read(|docs| docs.get_all::<SubmittedDelivery>())
        .await?
        .into_iter()
        .filter(SubmittedDelivery::is_terminal)
        .map(|row| row.tracking_number)
        .collect())
}

/// Both strings must be at least this long for a containment match.
const NEAR_DUPLICATE_MIN_LENGTH: usize = 8;

/// An equal known number, or one where both are
/// at least 8 characters and one contains the other (merchants truncate the
/// same shipment's number differently, e.g. `P5253806501` vs `P52538065`).
pub fn find_near_duplicate_tracking<'a, I>(candidate: &str, known_numbers: I) -> Option<&'a str>
where
    I: IntoIterator<Item = &'a str>,
{
    let candidate_len = omni_core::js::utf16_len(candidate);
    known_numbers.into_iter().find(|known| {
        *known == candidate
            || (omni_core::js::utf16_len(known) >= NEAR_DUPLICATE_MIN_LENGTH
                && candidate_len >= NEAR_DUPLICATE_MIN_LENGTH
                && (known.contains(candidate) || candidate.contains(*known)))
    })
}

/// Writes a terminal outcome.
pub async fn record(
    store: &Store,
    attempt: DeliveryAttempt,
    status: SubmissionStatus,
    attempts: Option<i64>,
) -> Result<SubmittedDelivery, StoreError> {
    store
        .write(move |tx| {
            let extra = tx
                .get::<SubmittedDelivery>(&attempt.tracking_number)?
                .map(|row| row.extra)
                .unwrap_or_default();
            let row = SubmittedDelivery {
                tracking_number: attempt.tracking_number,
                carrier_code: attempt.carrier_code,
                description: attempt.description,
                submitted_at: attempt.submitted_at,
                email_id: attempt.email_id,
                status: Some(status),
                attempts,
                origin: None,
                extra,
            };
            tx.upsert(&row, UpsertOpts::default())?;
            Ok::<_, StoreError>(row)
        })
        .await
}

/// Persists intent (`pending`, attempts + 1)
/// atomically before Parcel is called.
pub async fn reserve(
    store: &Store,
    attempt: DeliveryAttempt,
) -> Result<SubmittedDelivery, StoreError> {
    store
        .write(move |tx| {
            let prior = tx.get::<SubmittedDelivery>(&attempt.tracking_number)?;
            let row = SubmittedDelivery {
                tracking_number: attempt.tracking_number,
                carrier_code: attempt.carrier_code,
                description: attempt.description,
                submitted_at: attempt.submitted_at,
                email_id: attempt.email_id,
                status: Some(SubmissionStatus::Pending),
                attempts: Some(prior.as_ref().and_then(|p| p.attempts).unwrap_or(0) + 1),
                origin: None,
                extra: prior.map(|p| p.extra).unwrap_or_default(),
            };
            tx.upsert(&row, UpsertOpts::default())?;
            Ok::<_, StoreError>(row)
        })
        .await
}

/// Forgets a tracking number so a future email can resubmit it.
pub async fn forget(store: &Store, tracking_number: &str) -> Result<bool, StoreError> {
    let key = tracking_number.to_owned();
    store
        .write(move |tx| tx.delete::<SubmittedDelivery>(&key))
        .await
}

#[cfg(test)]
mod persistence_spec {
    use super::*;

    fn find<'a>(candidate: &str, known: &[&'a str]) -> Option<&'a str> {
        find_near_duplicate_tracking(candidate, known.iter().copied())
    }

    #[test]
    fn matches_an_exactly_equal_known_number() {
        assert_eq!(find("P5253806501", &["P5253806501"]), Some("P5253806501"));
    }

    #[test]
    fn matches_when_the_candidate_contains_a_known_number_both_at_least_8_chars() {
        assert_eq!(find("P5253806501", &["P52538065"]), Some("P52538065"));
    }

    #[test]
    fn matches_when_a_known_number_contains_the_candidate_both_at_least_8_chars() {
        assert_eq!(find("P52538065", &["P5253806501"]), Some("P5253806501"));
    }

    #[test]
    fn does_not_containment_match_when_the_candidate_is_shorter_than_8_chars() {
        assert_eq!(find("P525380", &["P5253806501"]), None);
    }

    #[test]
    fn does_not_containment_match_when_the_known_number_is_shorter_than_8_chars() {
        assert_eq!(find("P5253806501", &["P525380"]), None);
    }

    #[test]
    fn still_matches_short_strings_when_exactly_equal() {
        assert_eq!(find("ABC123", &["ABC123"]), Some("ABC123"));
    }

    #[test]
    fn returns_undefined_for_unrelated_numbers() {
        assert_eq!(
            find("1Z999AA10123456784", &["P5253806501", "9400111899"]),
            None
        );
    }

    #[test]
    fn returns_undefined_for_an_empty_known_set() {
        assert_eq!(find("P5253806501", &[]), None);
    }

    #[test]
    fn returns_the_first_matching_known_number() {
        assert_eq!(
            find("P5253806501", &["P52538065", "P5253806501"]),
            Some("P52538065")
        );
    }

    use omni_email::activity::{EmailActivityOutcome, derive_items_outcome};

    #[test]
    fn derives_processed_when_every_item_succeeded() {
        assert_eq!(
            derive_items_outcome(&[true, true]),
            EmailActivityOutcome::Processed
        );
    }

    #[test]
    fn derives_processed_when_items_were_dedup_skipped_treated_as_ok() {
        assert_eq!(
            derive_items_outcome(&[true]),
            EmailActivityOutcome::Processed
        );
    }

    #[test]
    fn derives_partial_when_some_items_failed() {
        assert_eq!(
            derive_items_outcome(&[true, false]),
            EmailActivityOutcome::Partial
        );
    }

    #[test]
    fn derives_failed_when_every_item_failed() {
        assert_eq!(
            derive_items_outcome(&[false, false]),
            EmailActivityOutcome::Failed
        );
    }

    #[test]
    fn derives_no_matches_for_an_empty_extraction() {
        assert_eq!(derive_items_outcome(&[]), EmailActivityOutcome::NoMatches);
    }
}
