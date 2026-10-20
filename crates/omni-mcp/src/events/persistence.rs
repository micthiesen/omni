//! MCP Events durable state.
//!
//! Entity structs mirror the stored shapes, including legacy fields (`folder`
//! on subscriptions and requests written before event arguments were generic),
//! and carry `extra` so read-modify-write never drops a field.

use indexmap::IndexMap;
use omni_store::cbor::{Extra, JsValue};
use omni_store::entity::{Entity, EntityOps, EntityWrite, UpsertOpts, pk};
use omni_store::{DocOps, Store, StoreError};
use serde::{Deserialize, Serialize};

use super::catalog::EventArguments;

/// Terminal delivery failures.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryFailure {
    SubscriptionInactive,
    CredentialsUnavailable,
    Rejected,
    AttemptsExhausted,
}

/// Why a pending delivery is held without an attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryWithhold {
    /// The owning gateway client or grant is revoked.
    AuthorizationInvalid,
    /// Executor could not be asked; only on rows from before the gateway.
    AuthorizationUnavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeliveryStatus {
    Pending,
    Delivered,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventRequestMethod {
    #[serde(rename = "events/list")]
    List,
    #[serde(rename = "events/subscribe")]
    Subscribe,
    #[serde(rename = "events/unsubscribe")]
    Unsubscribe,
}

/// `mcp-event-subscription`, keyed by `id`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventSubscription {
    pub id: String,
    pub owner: String,
    pub key_id: String,
    pub generation: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<EventArguments>,
    /// Rows written before event arguments were generic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    pub encrypted_url: String,
    pub encrypted_secret: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encrypted_previous_secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_secret_until: Option<i64>,
    /// An Executor delegated token, on rows from before the gateway; never written now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encrypted_authorization: Option<String>,
    /// Advertised refresh time (the expiry).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_before: Option<i64>,
    pub expires_at: i64,
    pub verified_at: i64,
    /// When the owner was told this subscription lapsed unrefreshed; a
    /// refresh writes a fresh row without it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lapse_alerted_at: Option<i64>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl EventSubscription {
    /// Stored arguments, else the legacy folder, else none.
    pub fn effective_arguments(&self) -> EventArguments {
        match (&self.arguments, &self.folder) {
            (Some(arguments), _) => arguments.clone(),
            (None, Some(folder)) if !folder.is_empty() => {
                IndexMap::from([("folder".to_owned(), folder.clone())])
            }
            _ => IndexMap::new(),
        }
    }
}

impl Entity for EventSubscription {
    const NAME: &'static str = "mcp-event-subscription";
    type Key = String;
    fn key(&self) -> String {
        self.id.clone()
    }
}

/// `mcp-event-receipt`: dedup marker for one source observation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventReceipt {
    pub message_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    pub received_at: i64,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Entity for EventReceipt {
    const NAME: &'static str = "mcp-event-receipt";
    type Key = String;
    fn key(&self) -> String {
        self.message_key.clone()
    }
}

/// `mcp-event-delivery`: one outbox row per subscription and event. Field
/// order matches the stored rows (optional keys come last).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventDelivery {
    pub id: String,
    pub subscription_id: String,
    pub owner: String,
    pub subscription_generation: String,
    pub event_id: String,
    pub name: String,
    pub timestamp: String,
    pub data: IndexMap<String, JsValue>,
    pub attempts: i64,
    pub next_attempt_at: i64,
    pub status: DeliveryStatus,
    pub created_at: i64,
    pub updated_at: i64,
    /// Pending without an attempt because the owner's access is revoked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub withheld: Option<DeliveryWithhold>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_status: Option<i64>,
    /// Transport failure without an HTTP status, such as a timeout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<DeliveryFailure>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl EventDelivery {
    /// The payload as JSON (what the webhook body and matchers read).
    pub fn data_json(&self) -> serde_json::Map<String, serde_json::Value> {
        self.data
            .iter()
            .map(|(key, value)| (key.clone(), js_to_json(value)))
            .collect()
    }
}

impl Entity for EventDelivery {
    const NAME: &'static str = "mcp-event-delivery";
    type Key = String;
    fn key(&self) -> String {
        self.id.clone()
    }
}

/// `mcp-event-request`: diagnostic record of an event RPC, without
/// credentials or callback paths.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventRequest {
    pub id: String,
    pub at: i64,
    pub method: EventRequestMethod,
    pub owner: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<EventArguments>,
    /// Requests recorded before event arguments were generic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callback_host: Option<String>,
    pub outcome: String,
    #[serde(flatten)]
    pub extra: Extra,
}

impl EventRequest {
    /// Stored arguments, else `{folder}` from a legacy row.
    pub fn effective_arguments(&self) -> Option<EventArguments> {
        match (&self.arguments, &self.folder) {
            (Some(arguments), _) => Some(arguments.clone()),
            (None, Some(folder)) if !folder.is_empty() => {
                Some(IndexMap::from([("folder".to_owned(), folder.clone())]))
            }
            _ => None,
        }
    }
}

impl Entity for EventRequest {
    const NAME: &'static str = "mcp-event-request";
    type Key = String;
    fn key(&self) -> String {
        self.id.clone()
    }
}

/// The newest event requests kept for diagnostics.
pub const MAX_EVENT_REQUESTS: usize = 30;

/// A JSON view of a stored value (`undefined` becomes `null`, dates their ms).
pub fn js_to_json(value: &JsValue) -> serde_json::Value {
    serde_json::to_value(value).unwrap_or(serde_json::Value::Null)
}

/// A stored value for a JSON payload.
pub fn json_to_js(value: &serde_json::Value) -> JsValue {
    match value {
        serde_json::Value::Null => JsValue::Null,
        serde_json::Value::Bool(b) => JsValue::Bool(*b),
        serde_json::Value::Number(n) => match n.as_i64() {
            Some(i) => JsValue::Int(i128::from(i)),
            None => JsValue::Float(n.as_f64().unwrap_or(f64::NAN)),
        },
        serde_json::Value::String(s) => JsValue::String(s.clone()),
        serde_json::Value::Array(items) => JsValue::Array(items.iter().map(json_to_js).collect()),
        serde_json::Value::Object(map) => JsValue::Object(
            map.iter()
                .map(|(key, value)| (key.clone(), json_to_js(value)))
                .collect(),
        ),
    }
}

/// Sort newest first: `b.at - a.at || b.id.localeCompare(a.id)`.
fn sort_requests(rows: &mut [EventRequest]) {
    rows.sort_by(|a, b| {
        b.at.cmp(&a.at)
            .then_with(|| omni_core::js::locale_compare(&b.id, &a.id))
    });
}

/// Each call is one store job.
#[derive(Clone)]
pub struct EventStore {
    store: Store,
}

impl EventStore {
    pub fn new(store: Store) -> Self {
        Self { store }
    }

    pub async fn subscriptions(&self) -> Result<Vec<EventSubscription>, StoreError> {
        self.store
            .read(|docs| docs.get_all::<EventSubscription>())
            .await
    }

    pub async fn subscription(&self, id: &str) -> Result<Option<EventSubscription>, StoreError> {
        let id = id.to_owned();
        self.store
            .read(move |docs| docs.get::<EventSubscription>(&id))
            .await
    }

    pub async fn upsert_subscription(&self, row: EventSubscription) -> Result<(), StoreError> {
        self.store
            .write(move |tx| tx.upsert(&row, UpsertOpts::default()))
            .await
    }

    pub async fn delete_subscription(&self, id: &str) -> Result<(), StoreError> {
        let id = id.to_owned();
        self.store
            .write(move |tx| tx.delete::<EventSubscription>(&id).map(|_| ()))
            .await
    }

    pub async fn receipt(&self, message_key: &str) -> Result<Option<EventReceipt>, StoreError> {
        let key = message_key.to_owned();
        self.store
            .read(move |docs| docs.get::<EventReceipt>(&key))
            .await
    }

    pub async fn receipts(&self) -> Result<Vec<EventReceipt>, StoreError> {
        self.store.read(|docs| docs.get_all::<EventReceipt>()).await
    }

    pub async fn delete_receipt(&self, message_key: &str) -> Result<(), StoreError> {
        let key = message_key.to_owned();
        self.store
            .write(move |tx| tx.delete::<EventReceipt>(&key).map(|_| ()))
            .await
    }

    /// Writes the receipt and every delivery in one transaction; `false` (and
    /// nothing written) when the receipt already exists.
    pub async fn commit_receipt_and_deliveries(
        &self,
        receipt: EventReceipt,
        deliveries: Vec<EventDelivery>,
    ) -> Result<bool, StoreError> {
        self.store
            .write(move |tx| {
                let receipt_pk = pk::<EventReceipt>(&receipt.message_key)?;
                if tx.get_raw_row(&receipt_pk)?.is_some() {
                    return Ok(false);
                }
                tx.upsert(&receipt, UpsertOpts::default())?;
                for delivery in &deliveries {
                    tx.upsert(delivery, UpsertOpts::default())?;
                }
                Ok::<_, StoreError>(true)
            })
            .await
    }

    pub async fn deliveries(&self) -> Result<Vec<EventDelivery>, StoreError> {
        self.store
            .read(|docs| docs.get_all::<EventDelivery>())
            .await
    }

    pub async fn delivery(&self, id: &str) -> Result<Option<EventDelivery>, StoreError> {
        let id = id.to_owned();
        self.store
            .read(move |docs| docs.get::<EventDelivery>(&id))
            .await
    }

    pub async fn upsert_delivery(&self, row: EventDelivery) -> Result<(), StoreError> {
        self.store
            .write(move |tx| tx.upsert(&row, UpsertOpts::default()))
            .await
    }

    pub async fn upsert_deliveries(&self, rows: Vec<EventDelivery>) -> Result<(), StoreError> {
        self.store
            .write(move |tx| {
                for row in &rows {
                    tx.upsert(row, UpsertOpts::default())?;
                }
                Ok::<_, StoreError>(())
            })
            .await
    }

    pub async fn delete_delivery(&self, id: &str) -> Result<(), StoreError> {
        let id = id.to_owned();
        self.store
            .write(move |tx| tx.delete::<EventDelivery>(&id).map(|_| ()))
            .await
    }

    /// Newest first (legacy `folder` rows decode to `arguments`).
    pub async fn requests(&self) -> Result<Vec<EventRequest>, StoreError> {
        let mut rows = self
            .store
            .read(|docs| docs.get_all::<EventRequest>())
            .await?;
        for row in &mut rows {
            if row.arguments.is_none() {
                row.arguments = row.effective_arguments();
            }
        }
        sort_requests(&mut rows);
        Ok(rows)
    }

    /// Records a request and keeps only the newest [`MAX_EVENT_REQUESTS`].
    pub async fn record_request(&self, row: EventRequest) -> Result<(), StoreError> {
        self.store
            .write(move |tx| {
                tx.upsert(&row, UpsertOpts::default())?;
                let mut rows = tx.get_all::<EventRequest>()?;
                sort_requests(&mut rows);
                for stale in rows.iter().skip(MAX_EVENT_REQUESTS) {
                    tx.delete::<EventRequest>(&stale.id)?;
                }
                Ok::<_, StoreError>(())
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_conversion_round_trips_payloads() {
        let payload = serde_json::json!({"messageId": "<a@b>", "uid": 42, "x": [true, null, 1.5]});
        assert_eq!(js_to_json(&json_to_js(&payload)), payload);
    }

    #[test]
    fn legacy_rows_expose_folder_arguments() {
        let request = EventRequest {
            id: "r".into(),
            at: 1,
            method: EventRequestMethod::Subscribe,
            owner: "legacy".into(),
            name: None,
            arguments: None,
            folder: Some("archive".into()),
            callback_host: None,
            outcome: "accepted".into(),
            extra: Extra::default(),
        };
        assert_eq!(
            request.effective_arguments(),
            Some(IndexMap::from([(
                "folder".to_owned(),
                "archive".to_owned()
            )]))
        );
    }
}
