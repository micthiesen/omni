//! `parcels_add`: one delivery submitted to Parcel through the email
//! pipeline's client, deduplicated against the pipeline's submissions and
//! counted against Parcel's daily add limit (`add_ledger`).
//!
//! The dedup check, the limit and a `pending` reservation commit in one
//! transaction before the request. A definite answer settles the
//! reservation: accepted (or Parcel already has it) confirms it as
//! `submitted`; a refusal restores the previous row, so a later email can
//! still submit the number. No answer leaves it `pending`, which neither path
//! resends automatically from this tool.

use std::collections::HashMap;
use std::sync::Arc;

use omni_core::js::to_iso_string;
use omni_core::spawn::must_complete;
use omni_http::SideEffectMode;
use omni_store::entity::{EntityOps as _, EntityWrite as _, UpsertOpts};
use omni_store::{Store, StoreError};
use serde::Serialize;
use tokio_util::task::TaskTracker;

use crate::add_ledger;
use crate::carriers::carrier_map::CarrierDirectory;
use crate::parcel_api::{AddResponse, ParcelApi, SubmitParams, SubmitResult};
use crate::persistence::{
    SubmissionOrigin, SubmissionStatus, SubmittedDelivery, find_near_duplicate_tracking,
};

const LOG: &str = "Main:ParcelTracker";
pub const MAX_TRACKING_CHARS: usize = 64;
pub const MAX_CARRIER_CHARS: usize = 20;
pub const MAX_DESCRIPTION_CHARS: usize = 100;
const MAX_SUGGESTIONS: usize = 8;
/// Parcel's `error_message` is third-party text; keep it short.
pub const MAX_PARCEL_MESSAGE_CHARS: usize = 300;

/// Input the tool rejects before anything is reserved or sent.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AddInputError {
    #[error("trackingNumber is empty after removing spaces")]
    EmptyTrackingNumber,
    #[error("trackingNumber may contain only letters, digits and hyphens")]
    TrackingNumberCharacters,
    #[error("trackingNumber is longer than {MAX_TRACKING_CHARS} characters")]
    TrackingNumberLength,
    #[error(
        "carrier must be a Parcel carrier code of 1-{MAX_CARRIER_CHARS} letters or digits, such as ups, fedex, usps, cp or puro"
    )]
    CarrierFormat,
    #[error("{}", unknown_carrier_message(.code, .suggestions))]
    UnknownCarrier {
        code: String,
        suggestions: Vec<String>,
    },
    #[error("description is longer than {MAX_DESCRIPTION_CHARS} characters")]
    DescriptionLength,
}

fn unknown_carrier_message(code: &str, suggestions: &[String]) -> String {
    let mut message = format!("Parcel does not support the carrier code {code:?}.");
    if suggestions.is_empty() {
        message.push_str(
            " Use a code from https://api.parcel.app/external/supported_carriers.json, such as ups, fedex, usps, dhl, cp (Canada Post) or puro (Purolator).",
        );
    } else {
        message.push_str(&format!(" Did you mean: {}?", suggestions.join(", ")));
    }
    message
}

#[derive(Debug, thiserror::Error)]
pub enum AddError {
    #[error(transparent)]
    Input(#[from] AddInputError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Trims and removes every whitespace character (`"1Z 999 AA1"` becomes
/// `"1Z999AA1"`); case is kept.
pub fn normalize_tracking_number(raw: &str) -> Result<String, AddInputError> {
    let number: String = raw.chars().filter(|c| !c.is_whitespace()).collect();
    if number.is_empty() {
        return Err(AddInputError::EmptyTrackingNumber);
    }
    if !number
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return Err(AddInputError::TrackingNumberCharacters);
    }
    if number.chars().count() > MAX_TRACKING_CHARS {
        return Err(AddInputError::TrackingNumberLength);
    }
    Ok(number)
}

/// Trimmed and lowercased; Parcel's codes are lowercase letters and digits.
pub fn normalize_carrier(raw: &str) -> Result<String, AddInputError> {
    let code = raw.trim().to_ascii_lowercase();
    if code.is_empty()
        || code.len() > MAX_CARRIER_CHARS
        || !code.chars().all(|c| c.is_ascii_alphanumeric())
    {
        return Err(AddInputError::CarrierFormat);
    }
    Ok(code)
}

/// Trimmed; blank means absent.
pub fn normalize_description(raw: Option<&str>) -> Result<Option<String>, AddInputError> {
    let Some(text) = raw.map(str::trim).filter(|text| !text.is_empty()) else {
        return Ok(None);
    };
    if text.chars().count() > MAX_DESCRIPTION_CHARS {
        return Err(AddInputError::DescriptionLength);
    }
    Ok(Some(text.to_owned()))
}

fn squash(text: &str) -> String {
    text.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Supported codes whose code or name resembles `code`, best first.
pub fn suggest_carriers(code: &str, supported: &HashMap<String, String>) -> Vec<String> {
    let wanted = squash(code);
    if wanted.len() < 2 {
        return Vec::new();
    }
    let mut matches: Vec<(u8, &str)> = supported
        .iter()
        .filter_map(|(candidate, name)| {
            let name = squash(name);
            let rank = if name == wanted {
                0
            } else if candidate.starts_with(&wanted) || wanted.starts_with(candidate.as_str()) {
                1
            } else if name.contains(&wanted) || (name.len() >= 3 && wanted.contains(&name)) {
                2
            } else {
                return None;
            };
            Some((rank, candidate.as_str()))
        })
        .collect();
    matches.sort();
    matches
        .into_iter()
        .take(MAX_SUGGESTIONS)
        .map(|(_, code)| code.to_owned())
        .collect()
}

/// Parcel's wording when the delivery is already in the app (HTTP 400).
pub fn is_already_added(message: &str) -> bool {
    message.to_ascii_lowercase().contains("already added")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AddStatus {
    Accepted,
    Duplicate,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AddErrorKind {
    NotConfigured,
    CarrierListUnavailable,
    InProgress,
    RateLimited,
    Rejected,
    Uncertain,
}

/// The tool's answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AddOutcome {
    pub status: AddStatus,
    pub tracking_number: String,
    pub carrier: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub carrier_name: Option<String>,
    pub description: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parcel_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<AddErrorKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duplicate_of: Option<String>,
}

/// One request, as the tool received it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddRequest {
    pub tracking_number: String,
    pub carrier: String,
    pub description: Option<String>,
    pub send_push_confirmation: bool,
}

enum Reservation {
    Reserved { prior: Option<SubmittedDelivery> },
    Duplicate { of: SubmittedDelivery },
    InProgress { of: String },
    Blocked { until: i64 },
}

/// Delivered (or legacy) rows: Parcel has these numbers.
fn in_parcel(row: &SubmittedDelivery) -> bool {
    matches!(row.status, None | Some(SubmissionStatus::Submitted))
}

fn reserve(
    tx: &mut omni_store::Tx<'_>,
    params: &SubmitParams,
    now: i64,
    count_request: bool,
) -> Result<Reservation, StoreError> {
    let rows = tx.get_all::<SubmittedDelivery>()?;
    let wanted = params.tracking_number.to_ascii_uppercase();
    for row in &rows {
        if row.tracking_number.to_ascii_uppercase() != wanted {
            continue;
        }
        match row.status {
            Some(SubmissionStatus::Pending) => {
                return Ok(Reservation::InProgress {
                    of: row.tracking_number.clone(),
                });
            }
            Some(SubmissionStatus::Rejected) => {}
            Some(SubmissionStatus::Submitted) | None => {
                return Ok(Reservation::Duplicate { of: row.clone() });
            }
        }
    }
    if let Some(near) = rows.iter().filter(|row| in_parcel(row)).find(|row| {
        let known = row.tracking_number.to_ascii_uppercase();
        find_near_duplicate_tracking(&wanted, std::iter::once(known.as_str())).is_some()
    }) {
        return Ok(Reservation::Duplicate { of: near.clone() });
    }
    if count_request && let Err(until) = add_ledger::take(tx, now)? {
        return Ok(Reservation::Blocked { until });
    }
    let prior = tx.get::<SubmittedDelivery>(&params.tracking_number)?;
    let row = SubmittedDelivery {
        tracking_number: params.tracking_number.clone(),
        carrier_code: params.carrier_code.clone(),
        description: params.description.clone(),
        submitted_at: now,
        email_id: String::new(),
        status: Some(SubmissionStatus::Pending),
        attempts: Some(prior.as_ref().and_then(|p| p.attempts).unwrap_or(0) + 1),
        origin: Some(SubmissionOrigin::ParcelsAdd),
        extra: prior.as_ref().map(|p| p.extra.clone()).unwrap_or_default(),
    };
    tx.upsert(&row, UpsertOpts::default())?;
    Ok(Reservation::Reserved { prior })
}

/// Whether the stored row is still this call's reservation (the pipeline
/// may have replaced it meanwhile; then it is the pipeline's to settle).
fn is_ours(row: Option<&SubmittedDelivery>, now: i64) -> bool {
    row.is_some_and(|row| {
        row.status == Some(SubmissionStatus::Pending)
            && row.origin == Some(SubmissionOrigin::ParcelsAdd)
            && row.submitted_at == now
    })
}

async fn confirm(store: &Store, tracking_number: String, now: i64) -> Result<(), StoreError> {
    store
        .write(move |tx| {
            let row = tx.get::<SubmittedDelivery>(&tracking_number)?;
            if let Some(mut row) = row.filter(|row| is_ours(Some(row), now)) {
                row.status = Some(SubmissionStatus::Submitted);
                tx.upsert(&row, UpsertOpts::default())?;
            }
            Ok(())
        })
        .await
}

/// Puts back what was there before the reservation (nothing, or a rejected
/// row), so a refused add never blocks a later email submission.
async fn release(
    store: &Store,
    tracking_number: String,
    prior: Option<SubmittedDelivery>,
    now: i64,
) -> Result<(), StoreError> {
    store
        .write(move |tx| {
            let row = tx.get::<SubmittedDelivery>(&tracking_number)?;
            if !is_ours(row.as_ref(), now) {
                return Ok(());
            }
            match prior {
                Some(prior) => tx.upsert(&prior, UpsertOpts::default()),
                None => tx.delete::<SubmittedDelivery>(&tracking_number).map(|_| ()),
            }
        })
        .await
}

/// Builds and settles `parcels_add` requests.
pub struct ParcelAdder {
    store: Store,
    api: Option<Arc<ParcelApi>>,
    carriers: Option<Arc<CarrierDirectory>>,
    tracker: TaskTracker,
}

impl ParcelAdder {
    /// `api` and `carriers` are `None` without `PARCEL_API_KEY`.
    pub fn new(
        store: Store,
        api: Option<Arc<ParcelApi>>,
        carriers: Option<Arc<CarrierDirectory>>,
        tracker: TaskTracker,
    ) -> Self {
        Self {
            store,
            api,
            carriers,
            tracker,
        }
    }

    pub async fn add(&self, request: AddRequest) -> Result<AddOutcome, AddError> {
        let tracking_number = normalize_tracking_number(&request.tracking_number)?;
        let carrier = normalize_carrier(&request.carrier)?;
        let description = normalize_description(request.description.as_deref())?;
        let outcome =
            |status, carrier_name: Option<String>, description: String, message| AddOutcome {
                status,
                tracking_number: tracking_number.clone(),
                carrier: carrier.clone(),
                carrier_name,
                description,
                message,
                parcel_message: None,
                error_kind: None,
                retry_after: None,
                duplicate_of: None,
            };
        let (Some(api), Some(carriers)) = (self.api.clone(), self.carriers.as_ref()) else {
            return Ok(AddOutcome {
                error_kind: Some(AddErrorKind::NotConfigured),
                ..outcome(
                    AddStatus::Error,
                    None,
                    description.unwrap_or_default(),
                    "Parcel is not configured on this Omni (PARCEL_API_KEY is unset); nothing was sent.".to_owned(),
                )
            });
        };
        let Some(supported) = carriers.supported().await else {
            return Ok(AddOutcome {
                error_kind: Some(AddErrorKind::CarrierListUnavailable),
                ..outcome(
                    AddStatus::Error,
                    None,
                    description.unwrap_or_default(),
                    "Parcel's carrier list could not be fetched, so the carrier cannot be validated; nothing was sent. Try again later.".to_owned(),
                )
            });
        };
        let Some(carrier_name) = supported.get(&carrier).cloned() else {
            let suggestions = suggest_carriers(&carrier, &supported);
            return Err(AddInputError::UnknownCarrier {
                code: carrier,
                suggestions,
            }
            .into());
        };
        let description = description.unwrap_or_else(|| format!("{carrier_name} package"));
        let params = SubmitParams {
            tracking_number: tracking_number.clone(),
            carrier_code: carrier.clone(),
            description: description.clone(),
        };
        let base = outcome(
            AddStatus::Error,
            Some(carrier_name),
            description,
            String::new(),
        );
        let store = self.store.clone();
        let push = request.send_push_confirmation;
        // Reservation, request and settlement finish even if the call is dropped.
        must_complete(&self.tracker, async move {
            submit(store, api, params, push, base).await
        })
        .await
    }
}

async fn submit(
    store: Store,
    api: Arc<ParcelApi>,
    params: SubmitParams,
    push: bool,
    base: AddOutcome,
) -> Result<AddOutcome, AddError> {
    let now = api.clock().now_ms();
    let live = api.mode() == SideEffectMode::Live;
    let reserve_params = params.clone();
    let reservation = store
        .write(move |tx| reserve(tx, &reserve_params, now, live))
        .await?;
    let prior = match reservation {
        Reservation::Reserved { prior } => prior,
        Reservation::Duplicate { of } => {
            let by = match of.origin {
                Some(SubmissionOrigin::ParcelsAdd) => "parcels_add",
                None => "the email pipeline",
            };
            return Ok(AddOutcome {
                status: AddStatus::Duplicate,
                message: format!(
                    "Omni already submitted {} to Parcel with carrier {} ({by}, {}); nothing was sent.",
                    of.tracking_number,
                    of.carrier_code,
                    to_iso_string(of.submitted_at)
                ),
                duplicate_of: Some(of.tracking_number),
                ..base
            });
        }
        Reservation::InProgress { of } => {
            return Ok(AddOutcome {
                error_kind: Some(AddErrorKind::InProgress),
                message: format!(
                    "A submission of {of} is in progress or its outcome was never confirmed; Omni does not resend it. Check the Parcel app."
                ),
                duplicate_of: Some(of),
                ..base
            });
        }
        Reservation::Blocked { until } => {
            return Ok(AddOutcome {
                error_kind: Some(AddErrorKind::RateLimited),
                message: format!(
                    "Parcel allows {} adds per day across Omni; parcels_add keeps the last {} for email and is paused until {}. Nothing was sent.",
                    add_ledger::PARCEL_DAILY_LIMIT,
                    add_ledger::PARCEL_DAILY_LIMIT - add_ledger::TOOL_CEILING,
                    to_iso_string(until)
                ),
                retry_after: Some(to_iso_string(until)),
                ..base
            });
        }
    };

    let AddResponse {
        result,
        status,
        error_message,
        retry_after_ms,
    } = api.add(&params, push).await;
    let error_message = error_message.map(|m| m.chars().take(MAX_PARCEL_MESSAGE_CHARS).collect());
    let tracking_number = params.tracking_number.clone();
    let refused_status = match result {
        SubmitResult::Success if error_message.is_none() => {
            confirm(&store, tracking_number, now).await?;
            let message = if live {
                "Parcel accepted the delivery.".to_owned()
            } else {
                "Recorded without calling Parcel (side effects are recorded on this instance)."
                    .to_owned()
            };
            tracing::info!(target: LOG, "parcels_add submitted {}", params.tracking_number);
            return Ok(AddOutcome {
                status: AddStatus::Accepted,
                message,
                ..base
            });
        }
        // A request timeout is not a refusal: Parcel may still have added it.
        SubmitResult::Rejected { status: 408 } | SubmitResult::Error => {
            let cause = status.map_or_else(
                || "no answer from Parcel".to_owned(),
                |status| format!("Parcel answered HTTP {status}"),
            );
            return Ok(AddOutcome {
                error_kind: Some(AddErrorKind::Uncertain),
                message: format!(
                    "The outcome is unknown ({cause}); the delivery may have been added. Omni keeps it reserved and will not resend it. Check the Parcel app."
                ),
                parcel_message: error_message,
                ..base
            });
        }
        SubmitResult::Success | SubmitResult::Rejected { .. } => status,
    };
    let http = refused_status.map_or_else(String::new, |s| format!(" (HTTP {s})"));
    if error_message.as_deref().is_some_and(is_already_added) {
        confirm(&store, tracking_number, now).await?;
        return Ok(AddOutcome {
            status: AddStatus::Duplicate,
            message: "Parcel already tracks this delivery; nothing changed.".to_owned(),
            parcel_message: error_message,
            ..base
        });
    }
    release(&store, tracking_number, prior, now).await?;
    if refused_status == Some(429) {
        let until = add_ledger::back_off(&store, now, retry_after_ms).await?;
        return Ok(AddOutcome {
            error_kind: Some(AddErrorKind::RateLimited),
            message: format!(
                "Parcel's rate limit was reached{http}; the delivery was not added. parcels_add is paused until {}.",
                to_iso_string(until)
            ),
            parcel_message: error_message,
            retry_after: Some(to_iso_string(until)),
            ..base
        });
    }
    let hint = if matches!(refused_status, Some(401 | 403)) {
        "Parcel refused Omni's API key; the owner must fix PARCEL_API_KEY."
    } else {
        "Check the tracking number and carrier."
    };
    Ok(AddOutcome {
        error_kind: Some(AddErrorKind::Rejected),
        message: format!("Parcel refused the delivery{http}; it was not added. {hint}"),
        parcel_message: error_message,
        ..base
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracking_numbers_lose_all_whitespace_and_keep_case() {
        assert_eq!(
            normalize_tracking_number("  1Z 999 aa1\t0123 ").unwrap(),
            "1Z999aa10123"
        );
        assert_eq!(
            normalize_tracking_number(" \t "),
            Err(AddInputError::EmptyTrackingNumber)
        );
        assert_eq!(
            normalize_tracking_number("1Z/999"),
            Err(AddInputError::TrackingNumberCharacters)
        );
        assert_eq!(
            normalize_tracking_number(&"A".repeat(MAX_TRACKING_CHARS + 1)),
            Err(AddInputError::TrackingNumberLength)
        );
    }

    #[test]
    fn carriers_are_lowercased_codes() {
        assert_eq!(normalize_carrier(" UPS ").unwrap(), "ups");
        assert_eq!(
            normalize_carrier("canada post"),
            Err(AddInputError::CarrierFormat)
        );
        assert_eq!(normalize_carrier(""), Err(AddInputError::CarrierFormat));
    }

    #[test]
    fn blank_descriptions_are_absent() {
        assert_eq!(normalize_description(Some("  ")).unwrap(), None);
        assert_eq!(
            normalize_description(Some(" Uniqlo order "))
                .unwrap()
                .as_deref(),
            Some("Uniqlo order")
        );
        assert_eq!(
            normalize_description(Some(&"x".repeat(MAX_DESCRIPTION_CHARS + 1))),
            Err(AddInputError::DescriptionLength)
        );
    }

    #[test]
    fn suggests_codes_by_name_and_prefix() {
        let supported: HashMap<String, String> = [
            ("cp", "Canada Post"),
            ("puro", "Purolator"),
            ("ups", "UPS"),
            ("amzlca", "Amazon Canada"),
        ]
        .into_iter()
        .map(|(c, n)| (c.to_owned(), n.to_owned()))
        .collect();
        assert_eq!(suggest_carriers("canadapost", &supported), ["cp"]);
        assert_eq!(suggest_carriers("purolator", &supported), ["puro"]);
        assert_eq!(suggest_carriers("amazon", &supported), ["amzlca"]);
        assert!(suggest_carriers("zz", &supported).is_empty());
    }

    #[test]
    fn recognizes_parcels_duplicate_answer() {
        assert!(is_already_added(
            "You have already added this delivery to the app"
        ));
        assert!(!is_already_added(
            "The tracking number submitted does not match any previously known formats"
        ));
    }
}
