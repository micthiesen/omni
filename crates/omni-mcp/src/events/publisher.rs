//! The [`EventPublisher`] port over the MCP Events outbox.
//!
//! Subsystems publish catalog events through this port without depending on
//! `omni-mcp`. Every publication goes through [`McpEventService::publish`], so
//! the receipt and outbox rows commit together under the short state lock,
//! delivery runs outside it, and the owning gateway client or grant is checked
//! at delivery.
//! Payloads are checked against the catalog's payload schema and size bound
//! before anything is stored.

use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;

use futures::future::BoxFuture;
use omni_runtime::ports::{EventPublication, EventPublisher, PortError};
use serde_json::Value;

use super::catalog::{EVENT_DEFINITIONS, event_definition};
use super::service::{McpEventService, PublishInput};

/// Largest serialized payload a port publication may carry.
pub const MAX_PAYLOAD_BYTES: usize = 4096;

/// The outbox receipt key: hashed, so dedup keys may hold identifiers.
pub fn receipt_key(name: &str, dedup_key: &str) -> String {
    omni_core::digest::sha256_hex(event_key(name, dedup_key))
}

/// The source of the stable event ID.
pub fn event_key(name: &str, dedup_key: &str) -> String {
    format!("{name}:{dedup_key}")
}

fn payload_validators() -> &'static HashMap<&'static str, jsonschema::Validator> {
    static VALIDATORS: OnceLock<HashMap<&'static str, jsonschema::Validator>> = OnceLock::new();
    VALIDATORS.get_or_init(|| {
        EVENT_DEFINITIONS
            .iter()
            .filter_map(|definition| {
                jsonschema::options()
                    .should_validate_formats(true)
                    .build(&definition.payload_schema())
                    .ok()
                    .map(|validator| (definition.name, validator))
            })
            .collect()
    })
}

fn rejected(message: String) -> PortError {
    PortError::Failed {
        message,
        transient: false,
    }
}

/// Checks a publication against the catalog without touching the outbox.
pub fn validate_publication(event: &EventPublication) -> Result<(), PortError> {
    if event_definition(event.name).is_none() {
        return Err(rejected(format!("unknown MCP event {}", event.name)));
    }
    if event.dedup_key.is_empty() {
        return Err(rejected(format!(
            "{} publication has no dedup key",
            event.name
        )));
    }
    let data = Value::Object(event.data.clone());
    let size = serde_json::to_vec(&data).map_or(usize::MAX, |bytes| bytes.len());
    if size > MAX_PAYLOAD_BYTES {
        return Err(rejected(format!(
            "{} payload is {size} bytes (limit {MAX_PAYLOAD_BYTES})",
            event.name
        )));
    }
    let valid = payload_validators()
        .get(event.name)
        .is_some_and(|validator| validator.is_valid(&data));
    if !valid {
        return Err(rejected(format!(
            "{} payload does not match its schema",
            event.name
        )));
    }
    Ok(())
}

/// Implements the port; cheap to clone.
#[derive(Clone)]
pub struct McpEventPublisher {
    events: McpEventService,
}

impl McpEventPublisher {
    pub fn new(events: McpEventService) -> Self {
        Self { events }
    }

    pub async fn publish(&self, event: &EventPublication) -> Result<bool, PortError> {
        validate_publication(event)?;
        self.events
            .publish(PublishInput {
                name: event.name.to_owned(),
                receipt_key: receipt_key(event.name, &event.dedup_key),
                event_key: event_key(event.name, &event.dedup_key),
                timestamp: omni_core::js::to_iso_string(event.occurred_at_ms),
                data: event.data.clone(),
            })
            .await
            .map_err(|error| PortError::Failed {
                message: format!("MCP event outbox: {error}"),
                transient: true,
            })
    }

    pub async fn active_arguments(
        &self,
        name: &str,
    ) -> Result<Vec<BTreeMap<String, String>>, PortError> {
        let rows = self
            .events
            .active_arguments(name)
            .await
            .map_err(|error| PortError::Failed {
                message: format!("MCP event subscriptions: {error}"),
                transient: true,
            })?;
        Ok(rows
            .into_iter()
            .map(|args| args.into_iter().collect())
            .collect())
    }
}

impl EventPublisher for McpEventPublisher {
    fn publish<'a>(
        &'a self,
        event: &'a EventPublication,
    ) -> BoxFuture<'a, Result<bool, PortError>> {
        Box::pin(McpEventPublisher::publish(self, event))
    }

    fn active_arguments<'a>(
        &'a self,
        name: &'a str,
    ) -> BoxFuture<'a, Result<Vec<BTreeMap<String, String>>, PortError>> {
        Box::pin(McpEventPublisher::active_arguments(self, name))
    }
}
