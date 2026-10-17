//! `omni compat-audit --db <copy>` (see docs/architecture.md, "Data
//! compatibility"): a read-only decode and health audit of a docstore copy.
//!
//! Per entity: rows, expired rows, NULL payloads, CBOR decode failures, typed
//! decode failures, `recompute_pk(data) != pk` mismatches, and the typed round
//! trip `decode -> typed -> encode -> decode`, which must read back as the
//! original JS value ([`cbor::same_value`]). Rows whose entity no subsystem
//! declares and legacy `$` rows with a NULL entity column are listed
//! separately.
//!
//! The source is copied into a temporary directory first; the given file is
//! never opened.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use omni_store::cbor::{self, JsValue};
use omni_store::entity::EntityDescriptor;

/// Every entity the binary persists: the foundation's plus each subsystem's.
pub fn entity_catalog() -> Vec<EntityDescriptor> {
    let mut all = vec![
        EntityDescriptor::of::<omni_tasks::persistence::TaskRunData>(),
        EntityDescriptor::of::<omni_tasks::persistence::TaskRunLog>(),
        EntityDescriptor::of::<omni_tasks::persistence::TaskScheduleState>(),
        EntityDescriptor::of::<omni_tasks::health::TaskHealthIncident>(),
        EntityDescriptor::of::<omni_ai::costs::CostEventData>(),
        EntityDescriptor::of::<omni_ai::costs::CostMigrationData>(),
        EntityDescriptor::of::<omni_ios_controls::persistence::IosControlRegistration>(),
        EntityDescriptor::of::<omni_ios_controls::devices::IosDevice>(),
        EntityDescriptor::of::<omni_ios_controls::devices::IosWidgetPush>(),
    ];
    for group in [
        omni_live::entities(),
        omni_live_intel::entities(),
        omni_personal::entities(),
        omni_presspods::entities(),
        omni_media::entity_descriptors(),
        omni_arr::entities(),
        omni_podcasts::entities(),
        omni_imap::subsystem::entities(),
        omni_parcel::entities(),
        omni_calendar::entities(),
        omni_email::entities(),
        omni_mcp::entities(),
    ] {
        for descriptor in group {
            if !all.iter().any(|d| d.name == descriptor.name) {
                all.push(descriptor);
            }
        }
    }
    all
}

/// Raw-key collections written with `upsert_doc` and an explicit entity
/// column: read as plain JS values, never typed or
/// migrated, so they are checked at the CBOR level only.
pub const RAW_COLLECTIONS: &[&str] = &[
    omni_imap::archive_store::ACTION_ENTITY,
    omni_imap::archive_store::MESSAGE_ENTITY,
    omni_imap::archive_store::HISTORY_ENTITY,
    omni_imap::compose::DRAFT_ENTITY,
    omni_imap::compose::SEND_ENTITY,
];

/// Per-entity counters.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EntityAudit {
    pub rows: u64,
    pub expired: u64,
    pub null_data: u64,
    pub cbor_failures: u64,
    pub typed_failures: u64,
    pub pk_mismatches: u64,
    /// Typed re-encode reads back as a different JS value.
    pub value_diffs: u64,
}

/// The audit of one database.
#[derive(Clone, Debug, Default)]
pub struct Audit {
    pub entities: BTreeMap<String, EntityAudit>,
    /// Entity names present in the database that no code reads (retired
    /// entities such as `jmap-email-state`); informational.
    pub orphans: BTreeMap<String, u64>,
    pub legacy_null_entity: u64,
    /// Up to 20 example failures (`pk: reason`).
    pub samples: Vec<String>,
}

impl Audit {
    /// Rows the running service would fail on or corrupt: CBOR and typed
    /// decode failures, key mismatches and value diffs.
    pub fn blocking(&self) -> u64 {
        self.entities
            .values()
            .map(|e| e.cbor_failures + e.typed_failures + e.pk_mismatches + e.value_diffs)
            .sum::<u64>()
    }

    /// A plain-text report.
    pub fn report(&self) -> String {
        let mut out = String::from(
            "entity                              rows  expired  null  cbor  typed  pk  diffs\n",
        );
        for (name, e) in &self.entities {
            out.push_str(&format!(
                "{name:<34} {:>6} {:>8} {:>5} {:>5} {:>6} {:>3} {:>6}\n",
                e.rows,
                e.expired,
                e.null_data,
                e.cbor_failures,
                e.typed_failures,
                e.pk_mismatches,
                e.value_diffs
            ));
        }
        for (name, rows) in &self.orphans {
            out.push_str(&format!(
                "orphan entity {name} (no reader): {rows} row(s)\n"
            ));
        }
        out.push_str(&format!(
            "legacy NULL-entity $ rows: {}\n",
            self.legacy_null_entity
        ));
        for sample in &self.samples {
            out.push_str(&format!("  {sample}\n"));
        }
        out.push_str(&format!("blocking issues: {}\n", self.blocking()));
        out
    }
}

struct Row {
    pk: String,
    data: Option<Vec<u8>>,
    entity: Option<String>,
    expires_at: rusqlite::types::Value,
}

fn integer(value: &rusqlite::types::Value) -> Option<i64> {
    match value {
        rusqlite::types::Value::Integer(i) => Some(*i),
        #[allow(clippy::cast_possible_truncation)]
        rusqlite::types::Value::Real(f) => Some(*f as i64),
        _ => None,
    }
}

fn read_rows(conn: &rusqlite::Connection) -> rusqlite::Result<Vec<Row>> {
    let mut statement =
        conn.prepare("SELECT pk, data, entity, expires_at FROM blobs ORDER BY pk")?;
    let rows = statement.query_map([], |row| {
        Ok(Row {
            pk: row.get("pk")?,
            data: row.get("data")?,
            entity: row.get("entity")?,
            expires_at: row.get("expires_at")?,
        })
    })?;
    rows.collect()
}

/// Audits `conn` (a copy).
pub fn audit(
    conn: &rusqlite::Connection,
    catalog: &[EntityDescriptor],
    now_ms: i64,
) -> anyhow::Result<Audit> {
    let mut audit = Audit::default();
    for row in read_rows(conn)? {
        if row.entity.is_none() && row.pk.starts_with('$') {
            audit.legacy_null_entity += 1;
        }
        let Some(name) = row.entity.as_deref() else {
            continue;
        };
        match catalog.iter().find(|d| d.name == name) {
            None if RAW_COLLECTIONS.contains(&name) => {
                let stats = audit.entities.entry(name.to_owned()).or_default();
                audit_raw_row(stats, &mut audit.samples, name, &row, now_ms);
            }
            None => *audit.orphans.entry(name.to_owned()).or_default() += 1,
            Some(descriptor) => {
                let stats = audit
                    .entities
                    .entry(descriptor.name.to_owned())
                    .or_default();
                audit_row(stats, &mut audit.samples, descriptor, &row, now_ms);
            }
        }
    }
    Ok(audit)
}

/// Decodes a raw-collection row with the type omni-imap reads it as.
fn decode_raw(entity: &str, pk: &str, value: JsValue) -> Result<(), String> {
    use omni_imap::{archive_store, compose};
    match entity {
        archive_store::ACTION_ENTITY => archive_store::decode_action(pk, value)
            .map(drop)
            .map_err(|e| e.to_string()),
        archive_store::MESSAGE_ENTITY => cbor::from_value::<String>(value)
            .map(drop)
            .map_err(|e| e.to_string()),
        archive_store::HISTORY_ENTITY => cbor::from_value::<Vec<String>>(value)
            .map(drop)
            .map_err(|e| e.to_string()),
        compose::DRAFT_ENTITY | compose::SEND_ENTITY => {
            cbor::from_value::<compose::StoredAttempt>(value)
                .map(drop)
                .map_err(|e| e.to_string())
        }
        other => Err(format!("no reader for raw collection {other}")),
    }
}

/// Audits one row of a raw collection: CBOR decode and a typed decode with
/// omni-imap's reader.
fn audit_raw_row(
    stats: &mut EntityAudit,
    samples: &mut Vec<String>,
    entity: &str,
    row: &Row,
    now_ms: i64,
) {
    stats.rows += 1;
    if integer(&row.expires_at).is_some_and(|at| at <= now_ms) {
        stats.expired += 1;
    }
    let Some(bytes) = &row.data else {
        stats.null_data += 1;
        return;
    };
    match cbor::decode(bytes) {
        Ok(value) => {
            if let Err(error) = decode_raw(entity, &row.pk, value) {
                stats.typed_failures += 1;
                sample(samples, &row.pk, format!("typed: {error}"));
            }
        }
        Err(error) => {
            stats.cbor_failures += 1;
            sample(samples, &row.pk, format!("cbor: {error}"));
        }
    }
}

fn sample(samples: &mut Vec<String>, pk: &str, reason: impl std::fmt::Display) {
    if samples.len() < 20 {
        samples.push(format!("{pk}: {reason}"));
    }
}

/// Audits one row of a known entity.
fn audit_row(
    stats: &mut EntityAudit,
    samples: &mut Vec<String>,
    descriptor: &EntityDescriptor,
    row: &Row,
    now_ms: i64,
) {
    stats.rows += 1;
    if integer(&row.expires_at).is_some_and(|at| at <= now_ms) {
        stats.expired += 1;
    }
    let Some(bytes) = &row.data else {
        stats.null_data += 1;
        return;
    };
    let original = match cbor::decode(bytes) {
        Ok(value) => value,
        Err(error) => {
            stats.cbor_failures += 1;
            sample(samples, &row.pk, format!("cbor: {error}"));
            return;
        }
    };
    let typed = match (descriptor.roundtrip)(&original) {
        Ok(value) => value,
        Err(error) => {
            stats.typed_failures += 1;
            sample(samples, &row.pk, format!("typed: {error}"));
            return;
        }
    };
    match (descriptor.recompute_pk)(&original) {
        Ok(pk) if pk == row.pk => {}
        Ok(pk) => {
            stats.pk_mismatches += 1;
            sample(samples, &row.pk, format!("recomputed key {pk}"));
        }
        Err(error) => {
            stats.pk_mismatches += 1;
            sample(samples, &row.pk, format!("key: {error}"));
        }
    }
    let reads_back = cbor::decode(&cbor::encode(&typed))
        .is_ok_and(|reread| cbor::same_value(&reread, &original));
    if !reads_back {
        stats.value_diffs += 1;
        sample(samples, &row.pk, "typed round trip changed the value");
    }
}

/// Parsed `compat-audit` arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditArgs {
    pub db: PathBuf,
}

/// Runs the subcommand: `(clean, report)`, where `clean` is false when
/// blocking issues were found.
pub fn run(args: &AuditArgs, now_ms: i64) -> anyhow::Result<(bool, String)> {
    use anyhow::Context as _;
    let dir = tempfile_dir()?;
    let copy = dir.join("audit-copy.db");
    let result = copy_database(&args.db, &copy).and_then(|()| {
        let conn = rusqlite::Connection::open(&copy).context("opening the copy")?;
        audit(&conn, &entity_catalog(), now_ms)
    });
    let _ = std::fs::remove_dir_all(&dir);
    let audit = result?;
    Ok((audit.blocking() == 0, audit.report()))
}

fn tempfile_dir() -> anyhow::Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("omni-compat-audit-{}", omni_core::ids::uuid_v4()));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Copies the database and its WAL/SHM side files (a WAL-mode copy may hold
/// committed pages only in `-wal`).
fn copy_database(from: &Path, to: &Path) -> anyhow::Result<()> {
    use anyhow::Context as _;
    std::fs::copy(from, to).with_context(|| format!("copying {}", from.display()))?;
    for suffix in ["-wal", "-shm"] {
        let side = PathBuf::from(format!("{}{suffix}", from.display()));
        if side.exists() {
            std::fs::copy(&side, PathBuf::from(format!("{}{suffix}", to.display())))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audits_typed_raw_orphan_and_corrupt_rows() {
        use omni_store::Entity as _;
        use omni_tasks::persistence::TaskScheduleState;
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE blobs (pk TEXT PRIMARY KEY, data BLOB, entity TEXT, \
             version INTEGER, expires_at INTEGER, updated_at INTEGER);",
        )
        .unwrap();
        let insert =
            |pk: &str, data: Option<Vec<u8>>, entity: Option<&str>, expires: Option<i64>| {
                conn.execute(
                    "INSERT INTO blobs (pk, data, entity, version, expires_at, updated_at) \
                 VALUES (?1, ?2, ?3, 0, ?4, 0)",
                    rusqlite::params![pk, data, entity, expires],
                )
                .unwrap();
            };
        let catalog = entity_catalog();
        let schedule = catalog
            .iter()
            .find(|d| d.name == TaskScheduleState::NAME)
            .unwrap();
        let state = JsValue::Object(
            [
                ("taskName".to_owned(), JsValue::String("T".to_owned())),
                (
                    "schedule".to_owned(),
                    JsValue::String("0 * * * *".to_owned()),
                ),
                ("evaluatedThrough".to_owned(), JsValue::Int(5)),
                ("future".to_owned(), JsValue::Undefined),
            ]
            .into_iter()
            .collect(),
        );
        let good_pk = (schedule.recompute_pk)(&state).unwrap();
        insert(
            &good_pk,
            Some(cbor::encode(&state)),
            Some(schedule.name),
            None,
        );
        insert(
            "$task-schedule-state#s1:U",
            Some(cbor::encode(&state)),
            Some(schedule.name),
            None,
        );
        insert("$x#bad", Some(vec![0xff, 0x00]), Some(schedule.name), None);
        insert("$x#null", None, Some(schedule.name), Some(1));
        let message = omni_imap::archive_store::MESSAGE_ENTITY;
        insert(
            "$m#1",
            Some(cbor::encode(&JsValue::String("id".to_owned()))),
            Some(message),
            None,
        );
        insert(
            "$gone#1",
            Some(cbor::encode(&JsValue::Null)),
            Some("gone"),
            None,
        );
        insert("$legacy", Some(cbor::encode(&JsValue::Null)), None, None);

        let audit = audit(&conn, &catalog, 10).unwrap();
        let stats = &audit.entities[schedule.name];
        assert_eq!(stats.rows, 4);
        assert_eq!(
            (stats.cbor_failures, stats.null_data, stats.expired),
            (1, 1, 1)
        );
        assert_eq!(
            (stats.typed_failures, stats.pk_mismatches, stats.value_diffs),
            (0, 1, 0)
        );
        assert_eq!(audit.entities[message].rows, 1);
        assert_eq!(audit.entities[message].typed_failures, 0);
        assert_eq!(audit.orphans["gone"], 1);
        assert_eq!(audit.legacy_null_entity, 1);
        assert_eq!(audit.blocking(), 2);
        assert!(audit.report().contains("blocking issues: 2"));
    }

    #[test]
    fn catalog_has_unique_names() {
        let catalog = entity_catalog();
        let mut names: Vec<_> = catalog.iter().map(|d| d.name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len());
        assert!(names.contains(&"task-run"));
        assert!(names.contains(&"streamer-status"));
    }
}
