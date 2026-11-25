//! The `blobs` docstore: one connection on a dedicated thread, reads and
//! `BEGIN IMMEDIATE` writes as jobs.
//!
//! Every read observes the latest commit because all jobs run in order on the
//! same connection. Reads hide expired rows (`expires_at <= now`) except
//! [`DocOps::get_raw_rows_by_prefix`]; prefix matches use `LIKE ... ESCAPE '\'`
//! with `%`, `_` and `\` escaped, which keeps SQLite's ASCII case-insensitive
//! matching.

use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use omni_core::clock::SharedClock;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use tokio::sync::oneshot;

use crate::StoreError;
use crate::cbor::{self, JsValue};
use crate::table::{Table, TableRow};

const LOG: &str = "Docstore";

/// Payloads above this size are written but warned about.
const LARGE_DOC_WARN_BYTES: usize = 256 * 1024;

/// Stack for the store thread; CBOR decoding recurses up to `cbor::MAX_DEPTH`.
const STORE_THREAD_STACK: usize = 16 * 1024 * 1024;

type Job = Box<dyn FnOnce(&mut Connection) + Send>;
type Panic = Box<dyn Any + Send>;

/// Handle to the store actor; cheap to clone. The thread exits once every
/// handle is dropped.
#[derive(Clone)]
pub struct Store {
    inner: Arc<StoreInner>,
}

struct StoreInner {
    jobs: mpsc::Sender<Job>,
    clock: SharedClock,
    path: PathBuf,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("path", &self.inner.path)
            .finish_non_exhaustive()
    }
}

/// Options for [`Store::open`].
#[derive(Clone)]
pub struct StoreOptions {
    /// `PRAGMA busy_timeout`; 5 s in production.
    pub busy_timeout: Duration,
    pub clock: SharedClock,
}

impl StoreOptions {
    /// Production defaults (5 s busy timeout) with the given clock.
    pub fn new(clock: SharedClock) -> Self {
        Self {
            busy_timeout: Duration::from_secs(5),
            clock,
        }
    }
}

pub(crate) fn sqlite_err(e: rusqlite::Error) -> StoreError {
    StoreError::Sqlite(e.to_string())
}

impl Store {
    /// Opens (creating if needed) the database: WAL, `synchronous=NORMAL`, and
    /// the schema of [`initialize_schema`].
    pub async fn open(path: &Path, opts: StoreOptions) -> Result<Store, StoreError> {
        let (jobs, receiver) = mpsc::channel::<Job>();
        let (opened_tx, opened_rx) = oneshot::channel();
        let thread_path = path.to_owned();
        let busy_timeout = opts.busy_timeout;
        std::thread::Builder::new()
            .name("omni-store".to_owned())
            .stack_size(STORE_THREAD_STACK)
            .spawn(move || {
                let mut conn = match open_connection(&thread_path, busy_timeout) {
                    Ok(conn) => {
                        let _ = opened_tx.send(Ok(()));
                        conn
                    }
                    Err(e) => {
                        let _ = opened_tx.send(Err(e));
                        return;
                    }
                };
                while let Ok(job) = receiver.recv() {
                    job(&mut conn);
                }
                if let Err((_, e)) = conn.close() {
                    tracing::warn!(target: LOG, "Closing sqlite failed: {e}");
                }
            })
            .map_err(|e| StoreError::Sqlite(format!("spawn store thread: {e}")))?;
        opened_rx.await.map_err(|_| StoreError::Closed)??;
        tracing::debug!(target: LOG, "Opened sqlite database {}", path.display());
        Ok(Store {
            inner: Arc::new(StoreInner {
                jobs,
                clock: opts.clock,
                path: path.to_owned(),
            }),
        })
    }

    /// The database file this store was opened on.
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// The store clock (the `now` every read and write uses).
    pub fn clock(&self) -> &SharedClock {
        &self.inner.clock
    }

    /// Runs `job` on the store thread. A panic inside `job` resumes on the caller.
    async fn run<R: Send + 'static>(
        &self,
        job: impl FnOnce(&mut Connection) -> R + Send + 'static,
    ) -> Result<R, StoreError> {
        let (done, result) = oneshot::channel::<Result<R, Panic>>();
        let job: Job = Box::new(move |conn| {
            let outcome = catch_unwind(AssertUnwindSafe(|| job(conn)));
            let _ = done.send(outcome);
        });
        self.inner.jobs.send(job).map_err(|_| StoreError::Closed)?;
        match result.await.map_err(|_| StoreError::Closed)? {
            Ok(value) => Ok(value),
            Err(panic) => resume_unwind(panic),
        }
    }

    /// Runs `f` on the store thread against the latest committed state.
    pub async fn read<R, F>(&self, f: F) -> Result<R, StoreError>
    where
        F: FnOnce(&Docs<'_>) -> Result<R, StoreError> + Send + 'static,
        R: Send + 'static,
    {
        // "now" is read on the calling task, where a paused test clock applies.
        let now = self.inner.clock.now_ms();
        self.run(move |conn| {
            let docs = Docs { conn, now };
            f(&docs)
        })
        .await?
    }

    /// Runs `f` inside `BEGIN IMMEDIATE ... COMMIT`; any `Err` (or panic) rolls back.
    pub async fn write<R, E, F>(&self, f: F) -> Result<R, E>
    where
        F: FnOnce(&mut Tx<'_>) -> Result<R, E> + Send + 'static,
        E: From<StoreError> + Send + 'static,
        R: Send + 'static,
    {
        let now = self.inner.clock.now_ms();
        self.run(move |conn| -> Result<R, E> {
            let txn = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(sqlite_err)?;
            let result = {
                let mut tx = Tx { conn: &txn, now };
                f(&mut tx)
            };
            match result {
                Ok(value) => {
                    txn.commit().map_err(sqlite_err)?;
                    Ok(value)
                }
                Err(e) => {
                    if let Err(rollback) = txn.rollback() {
                        tracing::warn!(target: LOG, "Rollback failed: {rollback}");
                    }
                    Err(e)
                }
            }
        })
        .await
        .map_err(E::from)?
    }

    /// WAL-safe online backup (`rusqlite::backup`) to a new file at `dest`.
    pub async fn backup_to(&self, dest: &Path) -> Result<(), StoreError> {
        let dest = dest.to_owned();
        self.run(move |conn| {
            let mut target = Connection::open(&dest).map_err(sqlite_err)?;
            let backup = rusqlite::backup::Backup::new(conn, &mut target).map_err(sqlite_err)?;
            backup
                .run_to_completion(256, Duration::from_millis(5), None)
                .map_err(sqlite_err)
        })
        .await?
    }

    /// Typed access to a relational table (pets); runs its `DDL` idempotently.
    pub async fn table<T: TableRow>(&self) -> Result<Table<T>, StoreError> {
        Table::<T>::make(self.clone()).await
    }

    /// Runs `f` with the raw connection on the store thread (relational tables,
    /// `PRAGMA` reads). Not for `blobs`: use [`Store::read`] / [`Store::write`].
    pub(crate) async fn with_connection<R, F>(&self, f: F) -> Result<R, StoreError>
    where
        F: FnOnce(&mut Connection) -> Result<R, StoreError> + Send + 'static,
        R: Send + 'static,
    {
        self.run(f).await?
    }
}

fn open_connection(path: &Path, busy_timeout: Duration) -> Result<Connection, StoreError> {
    let conn = Connection::open(path).map_err(sqlite_err)?;
    conn.busy_timeout(busy_timeout).map_err(sqlite_err)?;
    conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get::<_, String>(0))
        .map_err(sqlite_err)?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(sqlite_err)?;
    initialize_schema(&conn)?;
    Ok(conn)
}

/// Full schema for fresh files, additive `ALTER`s
/// (tolerating a concurrent "duplicate column") for old ones, both indexes.
fn initialize_schema(conn: &Connection) -> Result<(), StoreError> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS blobs (
      pk         TEXT PRIMARY KEY,
      entity     TEXT,
      version    INTEGER NOT NULL DEFAULT 0,
      expires_at INTEGER,
      updated_at INTEGER NOT NULL DEFAULT 0,
      data       BLOB
    )",
        [],
    )
    .map_err(sqlite_err)?;
    let columns: Vec<String> = {
        let mut stmt = conn
            .prepare("PRAGMA table_info(blobs)")
            .map_err(sqlite_err)?;
        stmt.query_map([], |row| row.get::<_, String>("name"))
            .map_err(sqlite_err)?
            .collect::<Result<_, _>>()
            .map_err(sqlite_err)?
    };
    for (name, def) in [
        ("entity", "entity TEXT"),
        ("version", "version INTEGER NOT NULL DEFAULT 0"),
        ("expires_at", "expires_at INTEGER"),
        ("updated_at", "updated_at INTEGER NOT NULL DEFAULT 0"),
    ] {
        if columns.iter().any(|c| c == name) {
            continue;
        }
        // A concurrent process may win the race to ALTER between our read and ours.
        if let Err(e) = conn.execute_batch(&format!("ALTER TABLE blobs ADD COLUMN {def}"))
            && !e
                .to_string()
                .to_lowercase()
                .contains("duplicate column name")
        {
            return Err(sqlite_err(e));
        }
    }
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS blobs_entity_idx ON blobs(entity);
         CREATE INDEX IF NOT EXISTS blobs_expiry_idx ON blobs(expires_at) WHERE expires_at IS NOT NULL;",
    )
    .map_err(sqlite_err)?;
    Ok(())
}

/// A `LIKE` prefix pattern: `%`, `_` and `\` match literally, then `%`.
pub fn like_prefix(prefix: &str) -> String {
    let mut out = String::with_capacity(prefix.len() + 1);
    for c in prefix.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

const NOT_EXPIRED: &str = "(expires_at IS NULL OR expires_at > ?2)";

/// Read view over the store connection. All reads apply the expiry filter
/// `(expires_at IS NULL OR expires_at > now)` except `get_raw_rows_by_prefix`.
pub struct Docs<'a> {
    conn: &'a Connection,
    now: i64,
}

/// Write view: an open `BEGIN IMMEDIATE` transaction.
pub struct Tx<'a> {
    conn: &'a Connection,
    now: i64,
}

impl<'a> Docs<'a> {
    /// The raw connection, for queries the docstore API does not cover.
    pub fn connection(&self) -> &'a Connection {
        self.conn
    }
}

impl<'a> Tx<'a> {
    /// The raw transaction connection, for statements the docstore API does not cover.
    pub fn connection(&self) -> &'a Connection {
        self.conn
    }
}

pub trait DocOps {
    fn now_ms(&self) -> i64;
    /// `CorruptRow` on undecodable payloads.
    fn get_doc(&self, pk: &str) -> Result<Option<JsValue>, StoreError>;
    fn get_raw_row(&self, pk: &str) -> Result<Option<RawRow>, StoreError>;
    fn has_doc(&self, pk: &str) -> Result<bool, StoreError>;
    /// `pk LIKE :p ESCAPE '\'` (ASCII case-insensitive, deliberately preserved).
    fn get_keys_by_prefix(&self, prefix: &str) -> Result<Vec<String>, StoreError>;
    /// Skips (and warns about) corrupt rows.
    fn get_docs_by_prefix(&self, prefix: &str) -> Result<Vec<(String, JsValue)>, StoreError>;
    /// Skips (and warns about) corrupt rows.
    fn get_docs_by_entity(&self, entity: &str) -> Result<Vec<(String, JsValue)>, StoreError>;
    fn count_by_prefix(&self, prefix: &str) -> Result<u64, StoreError>;
    fn count_by_entity(&self, entity: &str) -> Result<u64, StoreError>;
    /// Includes expired rows.
    fn get_raw_rows_by_prefix(&self, prefix: &str) -> Result<Vec<RawRow>, StoreError>;
    /// `SUM(LENGTH(data))` of matching rows; no expiry filter.
    fn storage_bytes_by_prefix(&self, prefix: &str) -> Result<u64, StoreError>;
    /// `page_count * page_size`.
    fn database_size_bytes(&self) -> Result<u64, StoreError>;
}

pub trait DocWrite: DocOps {
    /// Inserts or replaces the row; meta defaults: no entity, version 0, no
    /// expiry, `updated_at` = now.
    fn upsert_doc(&mut self, pk: &str, data: &JsValue, meta: DocMeta) -> Result<(), StoreError>;
    fn delete_doc(&mut self, pk: &str) -> Result<bool, StoreError>;
    /// Updates `expires_at` and `updated_at` of a live row.
    fn touch_doc(&mut self, pk: &str, expires_at: Option<i64>) -> Result<bool, StoreError>;
    fn delete_docs_by_prefix(&mut self, prefix: &str) -> Result<u64, StoreError>;
    fn delete_docs_by_entity(&mut self, entity: &str) -> Result<u64, StoreError>;
    /// Deletes rows of `entity` last written before `cutoff_ms` (rows with
    /// no `updated_at` count as old).
    fn delete_docs_by_entity_before(
        &mut self,
        entity: &str,
        cutoff_ms: i64,
    ) -> Result<u64, StoreError>;
    /// Physically deletes up to `limit` expired rows.
    fn cleanup_expired(&mut self, limit: u32) -> Result<u64, StoreError>;
    /// Deletes every `blobs` row.
    fn clear(&mut self) -> Result<(), StoreError>;
}

/// Metadata columns written with a document.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DocMeta {
    pub entity: Option<String>,
    pub version: i64,
    pub expires_at: Option<i64>,
    /// `None` writes the store clock's now.
    pub updated_at: Option<i64>,
}

/// One `blobs` row as stored; INTEGER columns also accept REAL values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawRow {
    pub pk: String,
    pub entity: Option<String>,
    pub version: i64,
    pub expires_at: Option<i64>,
    pub updated_at: i64,
    pub data: Option<Vec<u8>>,
}

impl RawRow {
    /// Decodes the payload; `CorruptRow` when it is NULL or not valid CBOR.
    pub fn decode(&self) -> Result<JsValue, StoreError> {
        decode_payload(&self.pk, self.data.as_deref())
    }
}

/// Decodes a payload with the failure attributed to `pk`.
pub(crate) fn decode_payload(pk: &str, data: Option<&[u8]>) -> Result<JsValue, StoreError> {
    let bytes = data.ok_or_else(|| StoreError::CorruptRow {
        pk: pk.to_owned(),
        reason: "NULL payload".to_owned(),
    })?;
    cbor::decode(bytes).map_err(|e| StoreError::CorruptRow {
        pk: pk.to_owned(),
        reason: e.to_string(),
    })
}

/// Reads an INTEGER column that may hold a REAL (or NULL) value.
fn opt_int(row: &rusqlite::Row<'_>, column: &str) -> rusqlite::Result<Option<i64>> {
    match row.get_ref(column)? {
        ValueRef::Null => Ok(None),
        ValueRef::Integer(n) => Ok(Some(n)),
        #[allow(clippy::cast_possible_truncation)]
        ValueRef::Real(x) => Ok(Some(x.trunc() as i64)),
        ValueRef::Text(_) | ValueRef::Blob(_) => Err(rusqlite::Error::InvalidColumnType(
            0,
            column.to_owned(),
            rusqlite::types::Type::Text,
        )),
    }
}

fn raw_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawRow> {
    Ok(RawRow {
        pk: row.get("pk")?,
        entity: row.get("entity")?,
        version: opt_int(row, "version")?.unwrap_or(0),
        expires_at: opt_int(row, "expires_at")?,
        updated_at: opt_int(row, "updated_at")?.unwrap_or(0),
        data: row.get("data")?,
    })
}

fn decode_rows(rows: Vec<(String, Option<Vec<u8>>)>) -> Vec<(String, JsValue)> {
    let mut out = Vec::with_capacity(rows.len());
    for (pk, data) in rows {
        match decode_payload(&pk, data.as_deref()) {
            Ok(value) => out.push((pk, value)),
            Err(e) => tracing::warn!(target: LOG, "Skipping {e}"),
        }
    }
    out
}

fn count(n: i64) -> u64 {
    u64::try_from(n).unwrap_or(0)
}

fn changes(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// The shared read implementation of `Docs` and `Tx`.
fn ops_get_raw_row(conn: &Connection, now: i64, pk: &str) -> Result<Option<RawRow>, StoreError> {
    conn.prepare_cached(&format!(
        "SELECT pk, entity, version, expires_at, updated_at, data FROM blobs
         WHERE pk = ?1 AND {NOT_EXPIRED}"
    ))
    .and_then(|mut stmt| stmt.query_row(params![pk, now], raw_row).optional())
    .map_err(sqlite_err)
}

fn ops_get_doc(conn: &Connection, now: i64, pk: &str) -> Result<Option<JsValue>, StoreError> {
    let data: Option<Option<Vec<u8>>> = conn
        .prepare_cached(&format!(
            "SELECT data FROM blobs WHERE pk = ?1 AND {NOT_EXPIRED}"
        ))
        .and_then(|mut stmt| {
            stmt.query_row(params![pk, now], |row| row.get(0))
                .optional()
        })
        .map_err(sqlite_err)?;
    data.map(|data| decode_payload(pk, data.as_deref()))
        .transpose()
}

fn ops_has_doc(conn: &Connection, now: i64, pk: &str) -> Result<bool, StoreError> {
    conn.prepare_cached(&format!(
        "SELECT 1 FROM blobs WHERE pk = ?1 AND {NOT_EXPIRED}"
    ))
    .and_then(|mut stmt| stmt.exists(params![pk, now]))
    .map_err(sqlite_err)
}

fn ops_keys_by_prefix(
    conn: &Connection,
    now: i64,
    prefix: &str,
) -> Result<Vec<String>, StoreError> {
    conn.prepare_cached(&format!(
        "SELECT pk FROM blobs WHERE pk LIKE ?1 ESCAPE '\\' AND {NOT_EXPIRED}"
    ))
    .and_then(|mut stmt| {
        stmt.query_map(params![like_prefix(prefix), now], |row| row.get(0))?
            .collect()
    })
    .map_err(sqlite_err)
}

fn ops_docs_where(
    conn: &Connection,
    now: i64,
    clause: &str,
    arg: &str,
) -> Result<Vec<(String, JsValue)>, StoreError> {
    let rows: Vec<(String, Option<Vec<u8>>)> = conn
        .prepare_cached(&format!(
            "SELECT pk, data FROM blobs WHERE {clause} AND {NOT_EXPIRED}"
        ))
        .and_then(|mut stmt| {
            stmt.query_map(params![arg, now], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect()
        })
        .map_err(sqlite_err)?;
    Ok(decode_rows(rows))
}

fn ops_count_where(
    conn: &Connection,
    now: i64,
    clause: &str,
    arg: &str,
) -> Result<u64, StoreError> {
    conn.prepare_cached(&format!(
        "SELECT COUNT(*) FROM blobs WHERE {clause} AND {NOT_EXPIRED}"
    ))
    .and_then(|mut stmt| stmt.query_row(params![arg, now], |row| row.get::<_, i64>(0)))
    .map(count)
    .map_err(sqlite_err)
}

fn ops_raw_rows_by_prefix(conn: &Connection, prefix: &str) -> Result<Vec<RawRow>, StoreError> {
    conn.prepare_cached(
        "SELECT pk, entity, version, expires_at, updated_at, data FROM blobs
         WHERE pk LIKE ?1 ESCAPE '\\'",
    )
    .and_then(|mut stmt| {
        stmt.query_map(params![like_prefix(prefix)], raw_row)?
            .collect()
    })
    .map_err(sqlite_err)
}

fn ops_storage_bytes(conn: &Connection, prefix: &str) -> Result<u64, StoreError> {
    conn.prepare_cached(
        "SELECT COALESCE(SUM(LENGTH(data)), 0) FROM blobs WHERE pk LIKE ?1 ESCAPE '\\'",
    )
    .and_then(|mut stmt| stmt.query_row(params![like_prefix(prefix)], |row| row.get::<_, i64>(0)))
    .map(count)
    .map_err(sqlite_err)
}

fn ops_database_size(conn: &Connection) -> Result<u64, StoreError> {
    let pages: i64 = conn
        .pragma_query_value(None, "page_count", |row| row.get(0))
        .map_err(sqlite_err)?;
    let size: i64 = conn
        .pragma_query_value(None, "page_size", |row| row.get(0))
        .map_err(sqlite_err)?;
    Ok(count(pages).saturating_mul(count(size)))
}

macro_rules! doc_ops {
    ($ty:ident) => {
        impl DocOps for $ty<'_> {
            fn now_ms(&self) -> i64 {
                self.now
            }
            fn get_doc(&self, pk: &str) -> Result<Option<JsValue>, StoreError> {
                ops_get_doc(self.conn, self.now, pk)
            }
            fn get_raw_row(&self, pk: &str) -> Result<Option<RawRow>, StoreError> {
                ops_get_raw_row(self.conn, self.now, pk)
            }
            fn has_doc(&self, pk: &str) -> Result<bool, StoreError> {
                ops_has_doc(self.conn, self.now, pk)
            }
            fn get_keys_by_prefix(&self, prefix: &str) -> Result<Vec<String>, StoreError> {
                ops_keys_by_prefix(self.conn, self.now, prefix)
            }
            fn get_docs_by_prefix(
                &self,
                prefix: &str,
            ) -> Result<Vec<(String, JsValue)>, StoreError> {
                ops_docs_where(
                    self.conn,
                    self.now,
                    "pk LIKE ?1 ESCAPE '\\'",
                    &like_prefix(prefix),
                )
            }
            fn get_docs_by_entity(
                &self,
                entity: &str,
            ) -> Result<Vec<(String, JsValue)>, StoreError> {
                ops_docs_where(self.conn, self.now, "entity = ?1", entity)
            }
            fn count_by_prefix(&self, prefix: &str) -> Result<u64, StoreError> {
                ops_count_where(
                    self.conn,
                    self.now,
                    "pk LIKE ?1 ESCAPE '\\'",
                    &like_prefix(prefix),
                )
            }
            fn count_by_entity(&self, entity: &str) -> Result<u64, StoreError> {
                ops_count_where(self.conn, self.now, "entity = ?1", entity)
            }
            fn get_raw_rows_by_prefix(&self, prefix: &str) -> Result<Vec<RawRow>, StoreError> {
                ops_raw_rows_by_prefix(self.conn, prefix)
            }
            fn storage_bytes_by_prefix(&self, prefix: &str) -> Result<u64, StoreError> {
                ops_storage_bytes(self.conn, prefix)
            }
            fn database_size_bytes(&self) -> Result<u64, StoreError> {
                ops_database_size(self.conn)
            }
        }
    };
}

doc_ops!(Docs);
doc_ops!(Tx);

impl DocWrite for Tx<'_> {
    fn upsert_doc(&mut self, pk: &str, data: &JsValue, meta: DocMeta) -> Result<(), StoreError> {
        let encoded = cbor::encode(data);
        self.conn
            .prepare_cached(
                "INSERT INTO blobs (pk, entity, version, expires_at, updated_at, data)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(pk) DO UPDATE SET
                   entity=excluded.entity,
                   version=excluded.version,
                   expires_at=excluded.expires_at,
                   updated_at=excluded.updated_at,
                   data=excluded.data",
            )
            .and_then(|mut stmt| {
                stmt.execute(params![
                    pk,
                    meta.entity,
                    meta.version,
                    meta.expires_at,
                    meta.updated_at.unwrap_or(self.now),
                    encoded,
                ])
            })
            .map_err(sqlite_err)?;
        if encoded.len() > LARGE_DOC_WARN_BYTES {
            tracing::warn!(
                target: LOG,
                "Large docstore payload for \"{pk}\": {} bytes (> {LARGE_DOC_WARN_BYTES}). \
                 Every collection read of this entity decodes it in full; consider trimming \
                 the row or moving heavy fields elsewhere.",
                encoded.len()
            );
        }
        Ok(())
    }

    fn delete_doc(&mut self, pk: &str) -> Result<bool, StoreError> {
        self.conn
            .prepare_cached("DELETE FROM blobs WHERE pk = ?1")
            .and_then(|mut stmt| stmt.execute(params![pk]))
            .map(|n| n > 0)
            .map_err(sqlite_err)
    }

    fn touch_doc(&mut self, pk: &str, expires_at: Option<i64>) -> Result<bool, StoreError> {
        self.conn
            .prepare_cached(&format!(
                "UPDATE blobs SET expires_at = ?3, updated_at = ?2
                 WHERE pk = ?1 AND {NOT_EXPIRED}"
            ))
            .and_then(|mut stmt| stmt.execute(params![pk, self.now, expires_at]))
            .map(|n| n > 0)
            .map_err(sqlite_err)
    }

    fn delete_docs_by_prefix(&mut self, prefix: &str) -> Result<u64, StoreError> {
        self.conn
            .prepare_cached("DELETE FROM blobs WHERE pk LIKE ?1 ESCAPE '\\'")
            .and_then(|mut stmt| stmt.execute(params![like_prefix(prefix)]))
            .map(changes)
            .map_err(sqlite_err)
    }

    fn delete_docs_by_entity(&mut self, entity: &str) -> Result<u64, StoreError> {
        self.conn
            .prepare_cached("DELETE FROM blobs WHERE entity = ?1")
            .and_then(|mut stmt| stmt.execute(params![entity]))
            .map(changes)
            .map_err(sqlite_err)
    }

    fn delete_docs_by_entity_before(
        &mut self,
        entity: &str,
        cutoff_ms: i64,
    ) -> Result<u64, StoreError> {
        self.conn
            .prepare_cached("DELETE FROM blobs WHERE entity = ?1 AND COALESCE(updated_at, 0) < ?2")
            .and_then(|mut stmt| stmt.execute(params![entity, cutoff_ms]))
            .map(changes)
            .map_err(sqlite_err)
    }

    fn cleanup_expired(&mut self, limit: u32) -> Result<u64, StoreError> {
        self.conn
            .prepare_cached(
                "DELETE FROM blobs WHERE pk IN (
                   SELECT pk FROM blobs
                   WHERE expires_at IS NOT NULL AND expires_at <= ?1
                   LIMIT ?2
                 )",
            )
            .and_then(|mut stmt| stmt.execute(params![self.now, limit]))
            .map(changes)
            .map_err(sqlite_err)
    }

    fn clear(&mut self) -> Result<(), StoreError> {
        self.conn
            .execute("DELETE FROM blobs", [])
            .map(|_| ())
            .map_err(sqlite_err)
    }
}
