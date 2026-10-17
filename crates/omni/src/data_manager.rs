//! Data manager: every managed entity's metadata,
//! counts and payload bytes, its rows (an undecodable row is listed as a
//! malformed placeholder instead of failing the listing), and guarded
//! single-row deletion by exact primary key.

use std::sync::Arc;

use omni_api::data::{
    DataRow, EntityRowsResponse, MALFORMED_ROW_KEY, MalformedRowMetadata, ManagedDataSummary,
    ManagedEntitySummary,
};
use omni_runtime::{AfterDelete, CanDelete, ManagedEntity};
use omni_store::cbor::JsValue;
use omni_store::entity::{EntityDescriptor, EntityWrite as _};
use omni_store::{DocOps as _, DocWrite as _};
use omni_store::{Store, StoreError};
use serde_json::{Map, Value};

use crate::json::{BigIntNotSerializable, js_value_to_json};

/// Display order of the managed entities. Slugs a subsystem adds beyond this
/// list follow in registration order.
pub const MANAGED_ORDER: &[&str] = &[
    "cost-event",
    "cost-migration",
    "task-run",
    "task-run-log",
    "task-schedule-state",
    "streamer-status",
    "streamer-sessions",
    "streamer-viewer-metrics",
    "streamer-platform-viewer-metrics",
    "live-profile-identity-link",
    "livestream-intelligence",
    "livestream-feedback",
    "livestream-diagnostics",
    "livestream-intelligence-event",
    "recs-recommendation-attempt",
    "podcast-recommendation-attempt",
    "podcast-taste-evidence",
    "podcast-taste-profile",
    "recs-taste-evidence",
    "recs-taste-profile",
    "recs-identity-alias",
    "press-pods-episode",
    "press-pods-job",
    "parcel-submitted-delivery",
    "calendar-created-event",
    "calendar-primary-change",
    "calendar-mcp-operation",
    "email-activity",
    "email-activity-log",
    "imap-folder-cursor",
    "jmap-email-dispatch",
    "email-retry",
    "email-replay",
    "email-systemic-alert",
    "email-sender-rule",
    "email-feedback",
];

/// Orders managed entities by [`MANAGED_ORDER`]; the first occurrence of
/// a slug wins.
pub fn order_managed(entities: Vec<ManagedEntity>) -> Vec<ManagedEntity> {
    let mut seen = std::collections::HashSet::new();
    let mut unique: Vec<ManagedEntity> = entities
        .into_iter()
        .filter(|e| seen.insert(e.slug))
        .collect();
    let rank = |slug: &str| {
        MANAGED_ORDER
            .iter()
            .position(|s| *s == slug)
            .unwrap_or(usize::MAX)
    };
    unique.sort_by_key(|e| rank(e.slug));
    unique
}

/// The foundation-owned rows (cost ledger and task runs).
pub fn foundation_entities() -> Vec<ManagedEntity> {
    use omni_ai::costs::{CostEventData, CostMigrationData};
    use omni_tasks::persistence::{TaskRunData, TaskRunLog, TaskScheduleState};
    let plain = |entity: EntityDescriptor,
                 label: &'static str,
                 description: &'static str,
                 warning: Option<&'static str>,
                 primary_key: &'static [&'static str]| ManagedEntity {
        slug: entity.name,
        label,
        description,
        warning,
        entity,
        primary_key,
        can_delete: None,
        after_delete: None,
    };
    let running_guard: CanDelete = Arc::new(|row: &JsValue| {
        (row.get("status").and_then(JsValue::as_str) == Some("running"))
            .then(|| "A running task cannot be deleted.".to_owned())
    });
    let delete_log: AfterDelete = Arc::new(|row: JsValue, store: Store| {
        Box::pin(async move {
            let Some(run_id) = row
                .get("runId")
                .and_then(JsValue::as_str)
                .map(str::to_owned)
            else {
                return Ok(());
            };
            store
                .write(move |tx| tx.delete::<TaskRunLog>(&run_id))
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
    });
    vec![
        plain(
            EntityDescriptor::of::<CostEventData>(),
            "Cost events",
            "Metered service usage and estimated cost ledger.",
            Some("Deleting events changes global cost totals and cannot be undone."),
            &["eventId"],
        ),
        plain(
            EntityDescriptor::of::<CostMigrationData>(),
            "Cost migration state",
            "Completed cost-ledger backfills and their imported event counts.",
            Some("Deleting this state can duplicate historical costs on the next restart."),
            &["version"],
        ),
        ManagedEntity {
            can_delete: Some(running_guard),
            after_delete: Some(delete_log),
            ..plain(
                EntityDescriptor::of::<TaskRunData>(),
                "Task runs",
                "Scheduled, manual, startup, and catch-up execution history.",
                Some("Deleting a run also deletes its stored log."),
                &["runId"],
            )
        },
        plain(
            EntityDescriptor::of::<TaskRunLog>(),
            "Task run logs",
            "Captured log lines for completed task runs.",
            None,
            &["runId"],
        ),
        plain(
            EntityDescriptor::of::<TaskScheduleState>(),
            "Task schedule state",
            "Last evaluated cron occurrence used for catch-up decisions.",
            Some("Deleting state changes the catch-up baseline for that task."),
            &["taskName"],
        ),
    ]
}

/// A data-manager failure (500).
#[derive(Debug, thiserror::Error)]
pub enum DataError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Json(#[from] BigIntNotSerializable),
    /// A key part that is not a string, finite number or boolean (`InvalidKeyError`).
    #[error("Invalid key for \"{entity}\": property \"{property}\"")]
    InvalidKey { entity: String, property: String },
    #[error("afterDelete failed: {0}")]
    AfterDelete(String),
}

/// `DeleteResult`.
#[derive(Clone, Debug, PartialEq)]
pub enum DeleteResult {
    Deleted(DataRow),
    InvalidKey,
    NotFound,
    Blocked(String),
}

/// The managed entities of one process.
#[derive(Clone)]
pub struct DataManager {
    store: Store,
    entities: Arc<Vec<ManagedEntity>>,
}

fn prefix(slug: &str) -> String {
    format!("${slug}#")
}

fn malformed_row(raw_key: &str, error: &str) -> DataRow {
    let mut meta = Map::new();
    meta.insert("rawKey".to_owned(), Value::String(raw_key.to_owned()));
    meta.insert("error".to_owned(), Value::String(error.to_owned()));
    let mut row = Map::new();
    row.insert(MALFORMED_ROW_KEY.to_owned(), Value::Object(meta));
    row
}

fn malformed_metadata(key: &Map<String, Value>) -> Option<MalformedRowMetadata> {
    if key.len() != 1 {
        return None;
    }
    let value = key.get(MALFORMED_ROW_KEY)?.as_object()?;
    Some(MalformedRowMetadata {
        raw_key: value.get("rawKey")?.as_str()?.to_owned(),
        error: value.get("error")?.as_str()?.to_owned(),
    })
}

/// `entity.getPk(key)`: `$<name>#` + `s<len>:<v>` / `n<num>` / `b0|b1` parts.
pub fn primary_key(
    slug: &str,
    props: &[&str],
    key: &Map<String, Value>,
) -> Result<String, DataError> {
    let mut parts = Vec::with_capacity(props.len());
    for prop in props {
        let invalid = || DataError::InvalidKey {
            entity: slug.to_owned(),
            property: (*prop).to_owned(),
        };
        let part = match key.get(*prop) {
            Some(Value::String(s)) => format!("s{}:{s}", omni_core::js::utf16_len(s)),
            Some(Value::Number(n)) => {
                let n = n.as_f64().ok_or_else(invalid)?;
                if !n.is_finite() {
                    return Err(invalid());
                }
                format!("n{}", omni_core::js::number_to_string(n))
            }
            Some(Value::Bool(b)) => if *b { "b1" } else { "b0" }.to_owned(),
            _ => return Err(invalid()),
        };
        parts.push(part);
    }
    Ok(format!("${slug}#{}", parts.join("#")))
}

fn row_json(row: &JsValue) -> Result<DataRow, DataError> {
    Ok(match js_value_to_json(row)? {
        Some(Value::Object(map)) => map,
        // Every entity payload is an object; anything else lists as itself
        // under no keys, as `{...value}` would.
        _ => Map::new(),
    })
}

impl DataManager {
    /// `entities` in display order (see [`order_managed`]).
    pub fn new(store: Store, entities: Vec<ManagedEntity>) -> Self {
        Self {
            store,
            entities: Arc::new(entities),
        }
    }

    pub fn slugs(&self) -> Vec<&'static str> {
        self.entities.iter().map(|e| e.slug).collect()
    }

    fn entity(&self, slug: &str) -> Option<&ManagedEntity> {
        self.entities.iter().find(|e| e.slug == slug)
    }

    fn summary_of(entity: &ManagedEntity, count: u64, storage_bytes: u64) -> ManagedEntitySummary {
        ManagedEntitySummary {
            slug: entity.slug.to_owned(),
            label: entity.label.to_owned(),
            description: entity.description.to_owned(),
            warning: entity.warning.map(str::to_owned),
            primary_key: entity.primary_key.iter().map(|s| (*s).to_owned()).collect(),
            count,
            storage_bytes,
        }
    }

    pub async fn list(&self) -> Result<Vec<ManagedEntitySummary>, DataError> {
        let slugs = self.slugs();
        let stats = self
            .store
            .read(move |docs| {
                slugs
                    .iter()
                    .map(|slug| {
                        Ok((
                            docs.count_by_entity(slug)?,
                            docs.storage_bytes_by_prefix(&prefix(slug))?,
                        ))
                    })
                    .collect::<Result<Vec<_>, StoreError>>()
            })
            .await?;
        Ok(self
            .entities
            .iter()
            .zip(stats)
            .map(|(entity, (count, bytes))| Self::summary_of(entity, count, bytes))
            .collect())
    }

    pub async fn storage(
        &self,
        entities: &[ManagedEntitySummary],
    ) -> Result<ManagedDataSummary, DataError> {
        let database_size_bytes = self.store.read(|docs| docs.database_size_bytes()).await?;
        Ok(ManagedDataSummary {
            database_size_bytes,
            entity_storage_bytes: entities.iter().map(|e| e.storage_bytes).sum(),
        })
    }

    /// `None` for an unknown slug.
    pub async fn rows(&self, slug: &str) -> Result<Option<EntityRowsResponse>, DataError> {
        let Some(entity) = self.entity(slug) else {
            return Ok(None);
        };
        let key_prefix = prefix(entity.slug);
        let (decoded, storage_bytes) = self
            .store
            .read(move |docs| {
                let mut rows = Vec::new();
                for raw_key in docs.get_keys_by_prefix(&key_prefix)? {
                    match docs.get_doc(&raw_key) {
                        Ok(Some(row)) => rows.push(Ok(row)),
                        Ok(None) => {}
                        Err(error) => rows.push(Err((raw_key, error.to_string()))),
                    }
                }
                Ok((rows, docs.storage_bytes_by_prefix(&key_prefix)?))
            })
            .await?;
        let mut rows = Vec::with_capacity(decoded.len());
        for row in decoded {
            rows.push(match row {
                Ok(row) => row_json(&row)?,
                Err((raw_key, error)) => malformed_row(&raw_key, &error),
            });
        }
        let count = u64::try_from(rows.len()).unwrap_or(u64::MAX);
        Ok(Some(EntityRowsResponse {
            summary: Self::summary_of(entity, count, storage_bytes),
            rows,
        }))
    }

    /// `None` for an unknown slug.
    pub async fn delete(
        &self,
        slug: &str,
        key: &Map<String, Value>,
    ) -> Result<Option<DeleteResult>, DataError> {
        let Some(entity) = self.entity(slug) else {
            return Ok(None);
        };
        let key_prefix = prefix(entity.slug);
        if let Some(malformed) = malformed_metadata(key) {
            let raw_key = malformed.raw_key.clone();
            let deleted = self
                .store
                .write(move |tx| -> Result<bool, StoreError> {
                    if !tx.get_keys_by_prefix(&key_prefix)?.contains(&raw_key) {
                        return Ok(false);
                    }
                    tx.delete_doc(&raw_key)
                })
                .await?;
            return Ok(Some(if deleted {
                DeleteResult::Deleted(malformed_row(&malformed.raw_key, &malformed.error))
            } else {
                DeleteResult::NotFound
            }));
        }
        let exact = key.len() == entity.primary_key.len()
            && entity
                .primary_key
                .iter()
                .all(|prop| key.contains_key(*prop));
        if !exact {
            return Ok(Some(DeleteResult::InvalidKey));
        }
        let pk = primary_key(entity.slug, entity.primary_key, key)?;
        let lookup = pk.clone();
        let Some(row) = self.store.read(move |docs| docs.get_doc(&lookup)).await? else {
            return Ok(Some(DeleteResult::NotFound));
        };
        if let Some(reason) = entity.can_delete.as_ref().and_then(|guard| guard(&row)) {
            return Ok(Some(DeleteResult::Blocked(reason)));
        }
        if !self.store.write(move |tx| tx.delete_doc(&pk)).await? {
            return Ok(Some(DeleteResult::NotFound));
        }
        let json = row_json(&row)?;
        if let Some(after) = &entity.after_delete {
            after(row, self.store.clone())
                .await
                .map_err(DataError::AfterDelete)?;
        }
        Ok(Some(DeleteResult::Deleted(json)))
    }
}
