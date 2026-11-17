//! Typed entities over the docstore.
//!
//! Primary keys are `$<name>#` plus `#`-joined parts (`s<utf16 len>:<value>`,
//! `n<Number#toString>`, `b1`/`b0`). Typed reads and writes run inside the
//! store's `read`/`write` jobs, so a read-modify-write is one transaction.

use indexmap::IndexMap;
use serde::Serialize;
use serde::de::DeserializeOwned;

use std::collections::HashSet;

use crate::StoreError;
use crate::cbor::{self, JsValue};
use crate::docstore::{DocMeta, DocOps, DocWrite, Tx};

/// One primary-key component.
#[derive(Clone, Debug, PartialEq)]
pub enum KeyPart {
    Str(String),
    Num(f64),
    Bool(bool),
}

impl From<&str> for KeyPart {
    fn from(value: &str) -> Self {
        KeyPart::Str(value.to_owned())
    }
}
impl From<String> for KeyPart {
    fn from(value: String) -> Self {
        KeyPart::Str(value)
    }
}
impl From<&String> for KeyPart {
    fn from(value: &String) -> Self {
        KeyPart::Str(value.clone())
    }
}
impl From<f64> for KeyPart {
    fn from(value: f64) -> Self {
        KeyPart::Num(value)
    }
}
impl From<i64> for KeyPart {
    fn from(value: i64) -> Self {
        #[allow(clippy::cast_precision_loss)]
        KeyPart::Num(value as f64)
    }
}
impl From<bool> for KeyPart {
    fn from(value: bool) -> Self {
        KeyPart::Bool(value)
    }
}

/// A primary key: the ordered values of the entity's pk properties.
pub trait EntityKey {
    fn parts(&self) -> Vec<KeyPart>;
}

impl EntityKey for String {
    fn parts(&self) -> Vec<KeyPart> {
        vec![KeyPart::Str(self.clone())]
    }
}
impl EntityKey for i64 {
    fn parts(&self) -> Vec<KeyPart> {
        vec![KeyPart::from(*self)]
    }
}
impl EntityKey for f64 {
    fn parts(&self) -> Vec<KeyPart> {
        vec![KeyPart::Num(*self)]
    }
}
impl EntityKey for bool {
    fn parts(&self) -> Vec<KeyPart> {
        vec![KeyPart::Bool(*self)]
    }
}
impl<A, B> EntityKey for (A, B)
where
    A: Clone + Into<KeyPart>,
    B: Clone + Into<KeyPart>,
{
    fn parts(&self) -> Vec<KeyPart> {
        vec![self.0.clone().into(), self.1.clone().into()]
    }
}
impl<A, B, C> EntityKey for (A, B, C)
where
    A: Clone + Into<KeyPart>,
    B: Clone + Into<KeyPart>,
    C: Clone + Into<KeyPart>,
{
    fn parts(&self) -> Vec<KeyPart> {
        vec![
            self.0.clone().into(),
            self.1.clone().into(),
            self.2.clone().into(),
        ]
    }
}

/// A typed document collection. Every entity struct also carries
/// `#[serde(flatten)] extra: cbor::Extra` so read-modify-write never drops fields.
pub trait Entity: Serialize + DeserializeOwned + Send + Sync + 'static {
    /// Entity name, e.g. `"streamer-status"`; must not contain `#`.
    const NAME: &'static str;
    const VERSION: i64 = 0;
    /// Used by `*-reset-delivery` (90 days) and `press-pods-job-attempt` (180 days).
    const DEFAULT_TTL_MS: Option<i64> = None;
    type Key: EntityKey + Send;
    fn key(&self) -> Self::Key;
    fn validate(&self) -> Result<(), String> {
        Ok(())
    }
    fn migrate(raw: JsValue, _from: i64) -> Result<JsValue, String> {
        Ok(raw)
    }
}

/// `s<utf16 len>:<value>`, `n<Number#toString>`, `b1`/`b0`.
fn encode_part(entity: &str, part: &KeyPart) -> Result<String, StoreError> {
    match part {
        KeyPart::Str(s) => Ok(format!("s{}:{s}", omni_core::js::utf16_len(s))),
        KeyPart::Num(n) if n.is_finite() => Ok(format!("n{}", omni_core::js::number_to_string(*n))),
        KeyPart::Num(n) => Err(StoreError::InvalidKey(format!(
            "{entity}: non-finite number key part {n}"
        ))),
        KeyPart::Bool(b) => Ok(if *b { "b1" } else { "b0" }.to_owned()),
    }
}

/// `$<name>#<part>#<part>...`; fails with `InvalidKey` on a non-finite number.
pub fn pk<E: Entity>(key: &E::Key) -> Result<String, StoreError> {
    let parts = key
        .parts()
        .iter()
        .map(|part| encode_part(E::NAME, part))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(format!("${}#{}", E::NAME, parts.join("#")))
}

/// Prefix for the leading key parts: `$<name>#` followed by `<part>#` for each
/// given part (the trailing `#` keeps `n1` from matching `n12`).
pub fn prefix<E: Entity>(partial: &[KeyPart]) -> Result<String, StoreError> {
    let mut out = format!("${}#", E::NAME);
    for part in partial {
        out.push_str(&encode_part(E::NAME, part)?);
        out.push('#');
    }
    Ok(out)
}

/// Shallow object patch for [`EntityWrite::patch`] (`{...current, ...partial}`).
pub type JsObjectPatch = IndexMap<String, JsValue>;

/// Expiry options for [`EntityWrite::upsert`] and [`EntityWrite::touch`]:
/// `expires_at` wins over `ttl_ms`, which wins over `Entity::DEFAULT_TTL_MS`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UpsertOpts {
    pub expires_at: Option<i64>,
    pub ttl_ms: Option<i64>,
}

/// Expiry override for update/patch: an explicit `expires_at` (where
/// `Some(None)` clears the expiry) wins over `ttl_ms`; with neither, the
/// row keeps its current expiry (a modify never applies the default TTL).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModifyOpts {
    pub expires_at: Option<Option<i64>>,
    pub ttl_ms: Option<i64>,
}

const LOG: &str = "Entity";

fn resolve_expiry<E: Entity>(now: i64, opts: UpsertOpts) -> Option<i64> {
    opts.expires_at
        .or_else(|| opts.ttl_ms.map(|ttl| now.saturating_add(ttl)))
        .or_else(|| E::DEFAULT_TTL_MS.map(|ttl| now.saturating_add(ttl)))
}

fn next_expiry(current: Option<i64>, now: i64, opts: ModifyOpts) -> Option<i64> {
    match (opts.expires_at, opts.ttl_ms) {
        (Some(explicit), _) => explicit,
        (None, Some(ttl)) => Some(now.saturating_add(ttl)),
        (None, None) => current,
    }
}

fn meta<E: Entity>(expires_at: Option<i64>, now: i64) -> DocMeta {
    DocMeta {
        entity: Some(E::NAME.to_owned()),
        version: E::VERSION,
        expires_at,
        updated_at: Some(now),
    }
}

fn decode_entity<E: Entity>(pk: &str, value: JsValue) -> Result<E, StoreError> {
    cbor::from_value(value).map_err(|source| StoreError::Decode {
        pk: pk.to_owned(),
        source,
    })
}

fn encode_entity<E: Entity>(pk: &str, entity: &E) -> Result<JsValue, StoreError> {
    cbor::to_value(entity).map_err(|source| StoreError::Encode {
        pk: pk.to_owned(),
        source,
    })
}

fn validate<E: Entity>(entity: &E) -> Result<(), StoreError> {
    entity.validate().map_err(|reason| StoreError::Validation {
        entity: E::NAME,
        reason,
    })
}

/// Decodes collection rows, skipping (and warning about) rows the typed
/// model rejects, as the docstore skips undecodable payloads.
fn decode_all<E: Entity>(rows: Vec<(String, JsValue)>) -> Vec<E> {
    rows.into_iter()
        .filter_map(|(pk, value)| match decode_entity::<E>(&pk, value) {
            Ok(entity) => Some(entity),
            Err(e) => {
                tracing::warn!(target: LOG, entity = E::NAME, "Skipping {e}");
                None
            }
        })
        .collect()
}

/// Typed reads; blanket-implemented for every [`DocOps`] (`Docs` and `Tx`).
pub trait EntityOps: DocOps {
    /// `CorruptRow` for an undecodable payload, `Decode` when the typed model rejects it.
    fn get<E: Entity>(&self, key: &E::Key) -> Result<Option<E>, StoreError> {
        let pk = pk::<E>(key)?;
        self.get_doc(&pk)?
            .map(|value| decode_entity::<E>(&pk, value))
            .transpose()
    }

    fn has<E: Entity>(&self, key: &E::Key) -> Result<bool, StoreError> {
        self.has_doc(&pk::<E>(key)?)
    }

    /// All live rows of the entity (by entity column); skips corrupt rows.
    fn get_all<E: Entity>(&self) -> Result<Vec<E>, StoreError> {
        Ok(decode_all(self.get_docs_by_entity(E::NAME)?))
    }

    /// Live rows whose key starts with the given leading parts; skips corrupt rows.
    fn get_by_prefix<E: Entity>(&self, partial: &[KeyPart]) -> Result<Vec<E>, StoreError> {
        Ok(decode_all(self.get_docs_by_prefix(&prefix::<E>(partial)?)?))
    }

    /// Live row count by entity column.
    fn count<E: Entity>(&self) -> Result<u64, StoreError> {
        self.count_by_entity(E::NAME)
    }
}

impl<T: DocOps + ?Sized> EntityOps for T {}

/// Typed writes; blanket-implemented for every [`DocWrite`] (`Tx`).
pub trait EntityWrite: DocWrite {
    /// Validates, then writes with the TTL rules of section 4.2.
    fn upsert<E: Entity>(&mut self, e: &E, opts: UpsertOpts) -> Result<(), StoreError> {
        validate(e)?;
        let pk = pk::<E>(&e.key())?;
        let value = encode_entity(&pk, e)?;
        let now = self.now_ms();
        self.upsert_doc(&pk, &value, meta::<E>(resolve_expiry::<E>(now, opts), now))
    }

    /// Read-modify-write inside the caller's transaction. `None` (and no
    /// write) when the row is absent. The callback may not move the row: a
    /// result with a different primary key fails with `Validation`.
    fn update<E: Entity>(
        &mut self,
        key: &E::Key,
        f: impl FnOnce(E) -> E,
        opts: ModifyOpts,
    ) -> Result<Option<E>, StoreError> {
        let pk = pk::<E>(key)?;
        let Some(raw) = self.get_raw_row(&pk)? else {
            return Ok(None);
        };
        let current = decode_entity::<E>(&pk, raw.decode()?)?;
        let next = f(current);
        self.write_modified(&pk, next, raw.expires_at, opts)
            .map(Some)
    }

    /// Shallow `{...current, ...partial}` merge, then the `update` rules.
    fn patch<E: Entity>(
        &mut self,
        key: &E::Key,
        partial: JsObjectPatch,
        opts: ModifyOpts,
    ) -> Result<Option<E>, StoreError> {
        let pk = pk::<E>(key)?;
        let Some(raw) = self.get_raw_row(&pk)? else {
            return Ok(None);
        };
        let mut current = raw.decode()?;
        let Some(object) = current.as_object_mut() else {
            return Err(StoreError::CorruptRow {
                pk,
                reason: "payload is not an object".to_owned(),
            });
        };
        for (field, value) in partial {
            object.insert(field, value);
        }
        let next = decode_entity::<E>(&pk, current)?;
        self.write_modified(&pk, next, raw.expires_at, opts)
            .map(Some)
    }

    #[doc(hidden)]
    fn write_modified<E: Entity>(
        &mut self,
        pk: &str,
        next: E,
        current_expiry: Option<i64>,
        opts: ModifyOpts,
    ) -> Result<E, StoreError> {
        let next_pk = self::pk::<E>(&next.key())?;
        if next_pk != pk {
            return Err(StoreError::Validation {
                entity: E::NAME,
                reason: format!("a modify may not change the primary key ({pk} -> {next_pk})"),
            });
        }
        validate(&next)?;
        let value = encode_entity(pk, &next)?;
        let now = self.now_ms();
        let expires_at = next_expiry(current_expiry, now, opts);
        self.upsert_doc(pk, &value, meta::<E>(expires_at, now))?;
        Ok(next)
    }

    fn delete<E: Entity>(&mut self, key: &E::Key) -> Result<bool, StoreError> {
        let pk = pk::<E>(key)?;
        self.delete_doc(&pk)
    }

    /// Deletes every row of the entity (by entity column).
    fn delete_all<E: Entity>(&mut self) -> Result<u64, StoreError> {
        self.delete_docs_by_entity(E::NAME)
    }

    /// Extends (or clears) the expiry of a live row without rewriting its
    /// payload; the default TTL applies when `opts` names none.
    fn touch<E: Entity>(&mut self, key: &E::Key, opts: UpsertOpts) -> Result<bool, StoreError> {
        let pk = pk::<E>(key)?;
        let now = self.now_ms();
        self.touch_doc(&pk, resolve_expiry::<E>(now, opts))
    }
}

impl<T: DocWrite + ?Sized> EntityWrite for T {}

/// Type-erased entity metadata for `migrate_all` and the compat audit.
#[derive(Clone, Copy, Debug)]
pub struct EntityDescriptor {
    pub name: &'static str,
    pub version: i64,
    pub recompute_pk: fn(&JsValue) -> Result<String, String>,
    pub migrate: fn(JsValue, i64) -> Result<JsValue, String>,
    /// Typed round trip `decode -> E -> encode` (compat audit).
    pub roundtrip: fn(&JsValue) -> Result<JsValue, String>,
}

fn recompute_pk_of<E: Entity>(value: &JsValue) -> Result<String, String> {
    let entity: E = cbor::from_value(value.clone()).map_err(|e| e.to_string())?;
    pk::<E>(&entity.key()).map_err(|e| e.to_string())
}

fn roundtrip_of<E: Entity>(value: &JsValue) -> Result<JsValue, String> {
    let entity: E = cbor::from_value(value.clone()).map_err(|e| e.to_string())?;
    cbor::to_value(&entity).map_err(|e| e.to_string())
}

impl EntityDescriptor {
    pub fn of<E: Entity>() -> Self {
        Self {
            name: E::NAME,
            version: E::VERSION,
            recompute_pk: recompute_pk_of::<E>,
            migrate: E::migrate,
            roundtrip: roundtrip_of::<E>,
        }
    }
}

/// Outcome of [`migrate_all`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MigrateReport {
    /// Rows rewritten (new key, payload version or metadata).
    pub migrated: u64,
    /// Rows left in place because their new key already existed.
    pub collisions_skipped: u64,
    /// Rows whose payload could not be decoded, migrated or re-keyed; left untouched.
    pub failed: u64,
}

/// `Entity.migrateAll()`: rewrites rows into the current key encoding, payload
/// version and metadata columns (including expired rows; collision skip;
/// `updated_at || now`). One failing row never aborts the migration: it is
/// warned about, counted and left in place. Runs inside the caller's
/// transaction.
pub fn migrate_all(
    tx: &mut Tx<'_>,
    entities: &[EntityDescriptor],
) -> Result<MigrateReport, StoreError> {
    let mut report = MigrateReport::default();
    for descriptor in entities {
        migrate_entity(tx, descriptor, &mut report)?;
    }
    tracing::debug!(
        target: LOG,
        "migrateAll rewrote {} rows across {} entities",
        report.migrated,
        entities.len()
    );
    Ok(report)
}

fn migrate_entity(
    tx: &mut Tx<'_>,
    descriptor: &EntityDescriptor,
    report: &mut MigrateReport,
) -> Result<(), StoreError> {
    let now = tx.now_ms();
    let rows = tx.get_raw_rows_by_prefix(&format!("${}#", descriptor.name))?;
    let mut claimed: HashSet<String> = rows.iter().map(|row| row.pk.clone()).collect();
    for row in rows {
        let prepared = row
            .decode()
            .map_err(|e| e.to_string())
            .and_then(|data| {
                if row.version < descriptor.version {
                    (descriptor.migrate)(data, row.version)
                } else {
                    Ok(data)
                }
            })
            .and_then(|data| (descriptor.recompute_pk)(&data).map(|new_pk| (data, new_pk)));
        let (data, new_pk) = match prepared {
            Ok(prepared) => prepared,
            Err(reason) => {
                report.failed += 1;
                tracing::warn!(target: LOG, "Skipping migration of \"{}\": {reason}", row.pk);
                continue;
            }
        };
        let unchanged = new_pk == row.pk
            && row.entity.as_deref() == Some(descriptor.name)
            && row.version == descriptor.version;
        if unchanged {
            continue;
        }
        if new_pk != row.pk && claimed.contains(&new_pk) {
            report.collisions_skipped += 1;
            tracing::warn!(
                target: LOG,
                "Skipping migration of \"{}\": target key \"{new_pk}\" is already occupied",
                row.pk
            );
            continue;
        }
        claimed.insert(new_pk.clone());
        tx.upsert_doc(
            &new_pk,
            &data,
            DocMeta {
                entity: Some(descriptor.name.to_owned()),
                version: descriptor.version,
                expires_at: row.expires_at,
                updated_at: Some(if row.updated_at == 0 {
                    now
                } else {
                    row.updated_at
                }),
            },
        )?;
        if new_pk != row.pk {
            tx.delete_doc(&row.pk)?;
        }
        report.migrated += 1;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Serialize, Deserialize)]
    struct Pair {
        a: String,
        b: f64,
    }

    impl Entity for Pair {
        const NAME: &'static str = "pair";
        type Key = (String, f64);
        fn key(&self) -> Self::Key {
            (self.a.clone(), self.b)
        }
    }

    #[test]
    fn keys_match_the_stored_encoding() {
        assert_eq!(
            pk::<Pair>(&("a#b😀".to_owned(), 1.5)).ok().as_deref(),
            Some("$pair#s5:a#b😀#n1.5")
        );
        assert!(matches!(
            pk::<Pair>(&("x".to_owned(), f64::NAN)),
            Err(StoreError::InvalidKey(_))
        ));
        assert_eq!(prefix::<Pair>(&[]).ok().as_deref(), Some("$pair#"));
        assert_eq!(
            prefix::<Pair>(&[KeyPart::from("x")]).ok().as_deref(),
            Some("$pair#s1:x#")
        );
        assert_eq!(EntityDescriptor::of::<Pair>().name, "pair");
    }
}
