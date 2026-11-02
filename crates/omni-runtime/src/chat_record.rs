//! Delivery bookkeeping for recommendations an assistant presents in chat
//! (`media_recommendation_record`, `podcast_recommendation_record`).
//!
//! A chat record is reserved before the assistant sends anything and is
//! finished only with the assistant's own send receipt. A receipt proves the
//! message was sent, not that it was read. An unknown send stays pending and
//! is never resent or withdrawn by Omni; only the caller can reconcile it,
//! either with a receipt or by withdrawing a reservation it knows was never
//! presented. Legacy pipeline rows never carry a chat record.

use omni_store::cbor::Extra;
use serde::{Deserialize, Serialize};

/// Receipts may trail the reservation's clock by this much.
const CLOCK_SKEW_MS: i64 = 5 * 60 * 1000;
pub const CHANNEL_MAX: usize = 100;
pub const MESSAGE_REF_MAX: usize = 300;
pub const PROVENANCE_MAX: usize = 500;
pub const REASON_MAX: usize = 500;

/// Who produced a recommendation row. Absent on every row the legacy
/// selection pipelines wrote.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecommendationOrigin {
    /// Reserved and finished by an assistant through a record tool.
    AssistantChat,
}

/// The origin as tools report it (`legacy_pipeline` for rows without one).
pub fn origin_label(origin: Option<RecommendationOrigin>) -> &'static str {
    match origin {
        None => "legacy_pipeline",
        Some(RecommendationOrigin::AssistantChat) => "assistant_chat",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatDelivery {
    /// Reserved; nothing has been sent.
    Reserved,
    /// The caller supplied a send receipt.
    Sent,
    /// The caller does not know whether the send happened.
    Unknown,
    /// The caller withdrew a reservation it knows was never presented.
    Withdrawn,
}

impl ChatDelivery {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reserved => "reserved",
            Self::Sent => "sent",
            Self::Unknown => "unknown",
            Self::Withdrawn => "withdrawn",
        }
    }
}

/// The caller's evidence that its chat message was sent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatReceipt {
    /// Where the message went, e.g. the chat surface's name.
    pub channel: String,
    /// The sent message's id in that channel.
    pub message_ref: String,
    /// Epoch ms the channel reported for the send.
    pub sent_at: i64,
    /// What produced the receipt (the send call or tool that returned it).
    pub provenance: String,
}

/// Chat bookkeeping stored on a recommendation row (`chatRecord`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatRecord {
    /// The caller's idempotency key for the reservation.
    pub caller_key: String,
    /// Bumped on every delivery change; finish and withdraw must name it.
    pub revision: i64,
    pub delivery: ChatDelivery,
    pub updated_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<ChatReceipt>,
    /// Why the caller reported an unknown send.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unknown_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub withdrawn_reason: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl ChatRecord {
    pub fn reserved(caller_key: String, now: i64) -> Self {
        Self {
            caller_key,
            revision: 1,
            delivery: ChatDelivery::Reserved,
            updated_at: now,
            receipt: None,
            unknown_reason: None,
            withdrawn_reason: None,
            extra: Extra::default(),
        }
    }
}

/// What the caller reports about its send.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum DeliveryReport {
    #[serde(rename_all = "camelCase")]
    Sent {
        channel: String,
        message_ref: String,
        sent_at: i64,
        provenance: String,
    },
    Unknown {
        reason: String,
    },
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ChatRecordError {
    #[error("{0}")]
    Input(String),
    #[error("Recommendation {0} was not recorded through the chat record tool")]
    NotChatRecord(String),
    #[error("Stale revision: expected {expected}, current revision is {current}")]
    StaleRevision { expected: i64, current: i64 },
    #[error("{0}")]
    Conflict(String),
}

/// The record after a finish or withdraw.
#[derive(Clone, Debug, PartialEq)]
pub enum Transition {
    /// The record already says exactly this (a retry); nothing to write.
    Unchanged,
    Changed(Box<ChatRecord>),
}

fn bounded(value: &str, field: &str, max: usize) -> Result<String, ChatRecordError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(ChatRecordError::Input(format!("{field} must not be empty")));
    }
    if trimmed.chars().count() > max {
        return Err(ChatRecordError::Input(format!(
            "{field} must be at most {max} characters"
        )));
    }
    Ok(trimmed.to_owned())
}

/// A validated report: receipts must be internally plausible against the
/// reservation time and the current clock.
pub fn validate_report(
    report: &DeliveryReport,
    reserved_at: i64,
    now: i64,
) -> Result<DeliveryReport, ChatRecordError> {
    match report {
        DeliveryReport::Sent {
            channel,
            message_ref,
            sent_at,
            provenance,
        } => {
            if *sent_at < reserved_at - CLOCK_SKEW_MS {
                return Err(ChatRecordError::Input(
                    "sentAt is before the recommendation was reserved".to_owned(),
                ));
            }
            if *sent_at > now + CLOCK_SKEW_MS {
                return Err(ChatRecordError::Input("sentAt is in the future".to_owned()));
            }
            Ok(DeliveryReport::Sent {
                channel: bounded(channel, "channel", CHANNEL_MAX)?,
                message_ref: bounded(message_ref, "messageRef", MESSAGE_REF_MAX)?,
                sent_at: *sent_at,
                provenance: bounded(provenance, "provenance", PROVENANCE_MAX)?,
            })
        }
        DeliveryReport::Unknown { reason } => Ok(DeliveryReport::Unknown {
            reason: bounded(reason, "reason", REASON_MAX)?,
        }),
    }
}

fn check_revision(record: &ChatRecord, expected: i64) -> Result<(), ChatRecordError> {
    if record.revision == expected {
        Ok(())
    } else {
        Err(ChatRecordError::StaleRevision {
            expected,
            current: record.revision,
        })
    }
}

fn changed(record: &ChatRecord, now: i64, edit: impl FnOnce(&mut ChatRecord)) -> Transition {
    let mut next = record.clone();
    edit(&mut next);
    next.revision = record.revision + 1;
    next.updated_at = now;
    Transition::Changed(Box::new(next))
}

/// Applies a validated report. A repeated identical report is `Unchanged`
/// whatever revision it names, so a retry after a lost response is safe.
pub fn finish(
    record: &ChatRecord,
    expected_revision: i64,
    report: &DeliveryReport,
    now: i64,
) -> Result<Transition, ChatRecordError> {
    match (record.delivery, report) {
        (
            ChatDelivery::Sent,
            DeliveryReport::Sent {
                channel,
                message_ref,
                sent_at,
                provenance,
            },
        ) => {
            let same = record.receipt.as_ref().is_some_and(|r| {
                r.channel == *channel
                    && r.message_ref == *message_ref
                    && r.sent_at == *sent_at
                    && r.provenance == *provenance
            });
            if same {
                Ok(Transition::Unchanged)
            } else {
                Err(ChatRecordError::Conflict(
                    "Already finished with a different send receipt".to_owned(),
                ))
            }
        }
        (ChatDelivery::Sent, DeliveryReport::Unknown { .. }) => Err(ChatRecordError::Conflict(
            "Already finished with a send receipt; an unknown report cannot replace it".to_owned(),
        )),
        (ChatDelivery::Withdrawn, _) => Err(ChatRecordError::Conflict(
            "The reservation was withdrawn; record a new one instead".to_owned(),
        )),
        (ChatDelivery::Unknown, DeliveryReport::Unknown { reason })
            if record.unknown_reason.as_deref() == Some(reason.as_str()) =>
        {
            Ok(Transition::Unchanged)
        }
        (
            ChatDelivery::Reserved | ChatDelivery::Unknown,
            DeliveryReport::Sent {
                channel,
                message_ref,
                sent_at,
                provenance,
            },
        ) => {
            check_revision(record, expected_revision)?;
            let receipt = ChatReceipt {
                channel: channel.clone(),
                message_ref: message_ref.clone(),
                sent_at: *sent_at,
                provenance: provenance.clone(),
            };
            Ok(changed(record, now, |next| {
                next.delivery = ChatDelivery::Sent;
                next.receipt = Some(receipt);
            }))
        }
        (ChatDelivery::Reserved | ChatDelivery::Unknown, DeliveryReport::Unknown { reason }) => {
            check_revision(record, expected_revision)?;
            let reason = reason.clone();
            Ok(changed(record, now, |next| {
                next.delivery = ChatDelivery::Unknown;
                next.unknown_reason = Some(reason);
            }))
        }
    }
}

/// Withdraws a reservation the caller knows was never presented. A sent
/// record can never be withdrawn.
pub fn withdraw(
    record: &ChatRecord,
    expected_revision: i64,
    reason: &str,
    now: i64,
) -> Result<Transition, ChatRecordError> {
    let reason = bounded(reason, "reason", REASON_MAX)?;
    match record.delivery {
        ChatDelivery::Withdrawn if record.withdrawn_reason.as_deref() == Some(reason.as_str()) => {
            Ok(Transition::Unchanged)
        }
        ChatDelivery::Withdrawn => Err(ChatRecordError::Conflict(
            "Already withdrawn with a different reason".to_owned(),
        )),
        ChatDelivery::Sent => Err(ChatRecordError::Conflict(
            "A sent recommendation was presented and cannot be withdrawn".to_owned(),
        )),
        ChatDelivery::Reserved | ChatDelivery::Unknown => {
            check_revision(record, expected_revision)?;
            Ok(changed(record, now, |next| {
                next.delivery = ChatDelivery::Withdrawn;
                next.withdrawn_reason = Some(reason);
            }))
        }
    }
}

/// The caller's idempotency key, trimmed and bounded.
pub fn caller_key(value: &str) -> Result<String, ChatRecordError> {
    bounded(value, "idempotencyKey", 200)
}

/// Bounded assistant prose (`whyForUser`).
pub fn prose(value: &str, field: &str, max: usize) -> Result<String, ChatRecordError> {
    bounded(value, field, max)
}

/// Chat bookkeeping as the record and read tools report it.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatRecordView {
    pub revision: i64,
    pub delivery: ChatDelivery,
    pub updated_at: i64,
    pub channel: Option<String>,
    pub message_ref: Option<String>,
    pub sent_at: Option<i64>,
    pub provenance: Option<String>,
    pub unknown_reason: Option<String>,
    pub withdrawn_reason: Option<String>,
}

impl From<&ChatRecord> for ChatRecordView {
    fn from(record: &ChatRecord) -> Self {
        let receipt = record.receipt.as_ref();
        Self {
            revision: record.revision,
            delivery: record.delivery,
            updated_at: record.updated_at,
            channel: receipt.map(|r| r.channel.clone()),
            message_ref: receipt.map(|r| r.message_ref.clone()),
            sent_at: receipt.map(|r| r.sent_at),
            provenance: receipt.map(|r| r.provenance.clone()),
            unknown_reason: record.unknown_reason.clone(),
            withdrawn_reason: record.withdrawn_reason.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000_000;

    fn sent(message_ref: &str) -> DeliveryReport {
        DeliveryReport::Sent {
            channel: "chat".into(),
            message_ref: message_ref.into(),
            sent_at: NOW,
            provenance: "send tool".into(),
        }
    }

    fn apply(record: &ChatRecord, transition: Transition) -> ChatRecord {
        match transition {
            Transition::Changed(next) => *next,
            Transition::Unchanged => record.clone(),
        }
    }

    #[test]
    fn a_receipt_finishes_a_reservation_once() {
        let record = ChatRecord::reserved("k".into(), NOW);
        let next = apply(&record, finish(&record, 1, &sent("m1"), NOW).unwrap());
        assert_eq!(next.delivery, ChatDelivery::Sent);
        assert_eq!(next.revision, 2);
        // A retry with the same receipt and the stale revision is a no-op.
        assert_eq!(
            finish(&next, 1, &sent("m1"), NOW).unwrap(),
            Transition::Unchanged
        );
        assert!(matches!(
            finish(&next, 2, &sent("m2"), NOW),
            Err(ChatRecordError::Conflict(_))
        ));
    }

    #[test]
    fn an_unknown_send_stays_unknown_until_reconciled() {
        let record = ChatRecord::reserved("k".into(), NOW);
        let unknown = DeliveryReport::Unknown {
            reason: "timeout".into(),
        };
        let next = apply(&record, finish(&record, 1, &unknown, NOW).unwrap());
        assert_eq!(next.delivery, ChatDelivery::Unknown);
        let sent_later = apply(&next, finish(&next, 2, &sent("m1"), NOW).unwrap());
        assert_eq!(sent_later.delivery, ChatDelivery::Sent);
        assert_eq!(sent_later.unknown_reason.as_deref(), Some("timeout"));
    }

    #[test]
    fn stale_revisions_are_rejected() {
        let record = ChatRecord::reserved("k".into(), NOW);
        assert_eq!(
            finish(&record, 7, &sent("m1"), NOW),
            Err(ChatRecordError::StaleRevision {
                expected: 7,
                current: 1
            })
        );
        assert!(matches!(
            withdraw(&record, 0, "never sent", NOW),
            Err(ChatRecordError::StaleRevision { .. })
        ));
    }

    #[test]
    fn a_sent_record_cannot_be_withdrawn_and_a_withdrawn_one_cannot_finish() {
        let record = ChatRecord::reserved("k".into(), NOW);
        let sent_record = apply(&record, finish(&record, 1, &sent("m1"), NOW).unwrap());
        assert!(matches!(
            withdraw(&sent_record, 2, "oops", NOW),
            Err(ChatRecordError::Conflict(_))
        ));
        let withdrawn = apply(&record, withdraw(&record, 1, "chat crashed", NOW).unwrap());
        assert_eq!(withdrawn.delivery, ChatDelivery::Withdrawn);
        assert_eq!(
            withdraw(&withdrawn, 1, "chat crashed", NOW).unwrap(),
            Transition::Unchanged
        );
        assert!(matches!(
            finish(&withdrawn, 2, &sent("m1"), NOW),
            Err(ChatRecordError::Conflict(_))
        ));
    }

    #[test]
    fn receipts_must_be_plausible() {
        let early = DeliveryReport::Sent {
            channel: "chat".into(),
            message_ref: "m".into(),
            sent_at: NOW - CLOCK_SKEW_MS - 1,
            provenance: "p".into(),
        };
        assert!(validate_report(&early, NOW, NOW).is_err());
        let future = DeliveryReport::Sent {
            channel: "chat".into(),
            message_ref: "m".into(),
            sent_at: NOW + CLOCK_SKEW_MS + 1,
            provenance: "p".into(),
        };
        assert!(validate_report(&future, NOW, NOW).is_err());
        let blank = DeliveryReport::Sent {
            channel: " ".into(),
            message_ref: "m".into(),
            sent_at: NOW,
            provenance: "p".into(),
        };
        assert!(validate_report(&blank, NOW, NOW).is_err());
    }
}
