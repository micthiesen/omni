//! `contacts-mcp-operation`: one idempotent contacts write, keyed by the
//! SHA-256 of its idempotency key. It holds no vCard text: steps keep
//! resource ids, preconditions, body digests and property digests (see
//! [`Expectation`]), enough to settle an uncertain write by reading.

use omni_store::cbor::Extra;
use omni_store::entity::Entity;
use serde::{Deserialize, Serialize};

use crate::edit::Expectation;

/// Confirmed, partial and failed operations are kept 30 days, uncertain 90.
pub const OPERATION_TTL_MS: i64 = 30 * 24 * 60 * 60 * 1000;
pub const UNCERTAIN_OPERATION_TTL_MS: i64 = 90 * 24 * 60 * 60 * 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OperationState {
    Reserved,
    Confirmed,
    /// Some independent steps applied, others were refused.
    Partial,
    Failed,
    Uncertain,
    /// A write was applied but the card read back does not match the plan
    /// (for example iCloud dropped an unedited property).
    Diverged,
    /// Writes were acknowledged but not yet read back.
    Unverified,
}

impl OperationState {
    pub fn as_str(self) -> &'static str {
        match self {
            OperationState::Reserved => "reserved",
            OperationState::Confirmed => "confirmed",
            OperationState::Partial => "partial",
            OperationState::Failed => "failed",
            OperationState::Uncertain => "uncertain",
            OperationState::Diverged => "diverged",
            OperationState::Unverified => "unverified",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StepKind {
    Put,
    Delete,
}

impl StepKind {
    pub fn method(self) -> &'static str {
        match self {
            StepKind::Put => "PUT",
            StepKind::Delete => "DELETE",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StepState {
    Planned,
    Sending,
    Acknowledged,
    Verified,
    Rejected,
    Uncertain,
    /// Not sent because an earlier step it depends on did not apply.
    Skipped,
    /// Applied, but the card read back differs from the plan.
    Diverged,
}

impl StepState {
    pub fn as_str(self) -> &'static str {
        match self {
            StepState::Planned => "planned",
            StepState::Sending => "sending",
            StepState::Acknowledged => "acknowledged",
            StepState::Verified => "verified",
            StepState::Rejected => "rejected",
            StepState::Uncertain => "uncertain",
            StepState::Skipped => "skipped",
            StepState::Diverged => "diverged",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationStep {
    pub kind: StepKind,
    /// `<bookId>/<resource>`.
    pub resource_id: String,
    /// What the step does, e.g. `update contact` or `add members`.
    pub purpose: String,
    /// `if-match:<etag>` or `if-none-match`.
    pub precondition: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_sha256: Option<String>,
    pub state: StepState,
    /// The ETag the write's own answer carried (the version it produced).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub put_etag: Option<String>,
    /// The ETag of the version last read back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Property names and actions (no values).
    #[serde(default)]
    pub changes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect: Option<Expectation>,
    /// Read back and matching the plan (false when unverified or different).
    #[serde(default)]
    pub verified: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationErrorRecord {
    pub code: String,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationRecord {
    pub key_hash: String,
    pub idempotency_key: String,
    pub fingerprint: String,
    pub tool: String,
    pub state: OperationState,
    pub steps: Vec<OperationStep>,
    /// Steps after a refused or uncertain one are skipped (dependent steps).
    #[serde(default)]
    pub dependent: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<OperationErrorRecord>,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Entity for OperationRecord {
    const NAME: &'static str = "contacts-mcp-operation";
    type Key = String;
    fn key(&self) -> String {
        self.key_hash.clone()
    }
}

impl OperationRecord {
    pub fn ttl_ms(&self) -> Option<i64> {
        match self.state {
            OperationState::Reserved => None,
            OperationState::Uncertain | OperationState::Diverged | OperationState::Unverified => {
                Some(UNCERTAIN_OPERATION_TTL_MS)
            }
            OperationState::Confirmed | OperationState::Partial | OperationState::Failed => {
                Some(OPERATION_TTL_MS)
            }
        }
    }

    /// The state the steps add up to. Only a step read back matching its
    /// plan counts as done; an acknowledged one is unverified until read.
    pub fn settled_state(&self) -> OperationState {
        let any = |state: StepState| self.steps.iter().any(|s| s.state == state);
        if any(StepState::Uncertain) || any(StepState::Sending) {
            OperationState::Uncertain
        } else if any(StepState::Diverged) {
            OperationState::Diverged
        } else if any(StepState::Acknowledged) {
            OperationState::Unverified
        } else if self.steps.iter().all(|s| s.state == StepState::Verified) {
            OperationState::Confirmed
        } else if any(StepState::Verified) {
            OperationState::Partial
        } else {
            OperationState::Failed
        }
    }
}

/// `contacts-recovery-copy`: the card as it was before a write, kept
/// privately (never served by a tool) until the write reads back matching
/// its plan, so a write that did not keep the card intact can be undone by
/// hand. Bounded by its TTL.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryCopy {
    /// `<operation key hash>:<step index>`.
    pub key: String,
    pub resource_id: String,
    pub etag: String,
    pub vcard: String,
    pub created_at: i64,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Entity for RecoveryCopy {
    const NAME: &'static str = "contacts-recovery-copy";
    type Key = String;
    fn key(&self) -> String {
        self.key.clone()
    }
}

/// Recovery copies are kept 30 days.
pub const RECOVERY_TTL_MS: i64 = 30 * 24 * 60 * 60 * 1000;

pub fn recovery_key(key_hash: &str, step: usize) -> String {
    format!("{key_hash}:{step}")
}
