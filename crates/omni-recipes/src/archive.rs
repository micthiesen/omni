//! The library archive: every row of every recipe table as plain JSON, with
//! a SHA-256 per table, gzipped. `RecipesBackup` writes one an hour when the
//! library changed, so the file-level off-site backup (which copies the live
//! SQLite file without a consistent snapshot) always holds a restorable copy.
//! Image files need no archive: they are immutable and written atomically.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use rusqlite::Connection;
use rusqlite::types::{Value as Sql, ValueRef};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::db::{self, iso, sha256_hex};
use crate::error::{R, corrupt, io, reject, sql};
use crate::files::Rendition;
use crate::schema::{self, SCHEMA_VERSION, TABLES, meta_u64, set_meta};
use crate::service::Recipes;

pub const ARCHIVE_FORMAT: &str = "fond-archive";
pub const ARCHIVE_VERSION: u32 = 1;
/// Archives kept in the archive directory.
pub const KEEP: usize = 72;
/// Days an archive written before a purge is kept.
pub const PURGE_ARCHIVE_DAYS: i64 = 180;
const MAX_ARCHIVE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Archive {
    pub format: String,
    pub version: u32,
    pub archive_id: String,
    pub created_at: String,
    pub schema_version: u32,
    /// The library sequence number the archive holds.
    pub seq: u64,
    pub cook_seq: u64,
    /// Rows per table.
    pub counts: Map<String, Value>,
    /// SHA-256 of each table's rows as written.
    pub checksums: Map<String, Value>,
    /// Every image digest the rows refer to.
    pub image_digests: Vec<String>,
    pub tables: Map<String, Value>,
}

/// What a verification found.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Verified {
    pub archive_id: String,
    pub seq: u64,
    pub created_at: String,
    pub counts: Map<String, Value>,
    /// Image files checked, when a recipes directory was given.
    pub images_checked: Option<u64>,
}

fn archive_error(message: impl Into<String>) -> crate::error::RecipesError {
    reject("archive", message)
}

fn checksum(rows: &Value) -> String {
    sha256_hex(rows.to_string().as_bytes())
}

/// Every row of `table`, each as `{column: value}`, in primary-key order.
fn dump_table(conn: &Connection, table: &str) -> R<Value> {
    let mut statement = conn
        .prepare(&format!("SELECT * FROM {table} ORDER BY 1, 2"))
        .map_err(sql)?;
    let columns: Vec<String> = statement.column_names().map(str::to_owned).collect();
    let mut rows = statement.query([]).map_err(sql)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(sql)? {
        let mut object = Map::new();
        for (index, column) in columns.iter().enumerate() {
            let value = match row.get_ref(index).map_err(sql)? {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(value) => json!(value),
                ValueRef::Real(value) => json!(value),
                ValueRef::Text(text) => json!(String::from_utf8_lossy(text)),
                ValueRef::Blob(bytes) => json!(hex::encode(bytes)),
            };
            object.insert(column.clone(), value);
        }
        out.push(Value::Object(object));
    }
    Ok(Value::Array(out))
}

/// Every table's rows, by table name.
pub fn dump(conn: &Connection) -> R<Map<String, Value>> {
    let mut tables = Map::new();
    for table in TABLES {
        tables.insert((*table).to_owned(), dump_table(conn, table)?);
    }
    Ok(tables)
}

/// Every image digest the library refers to: recipe images, inbox photos and
/// stored lead images.
pub fn referenced_digests(conn: &Connection) -> R<Vec<String>> {
    let mut statement = conn
        .prepare(
            "SELECT sha256 FROM recipes_image
             UNION SELECT sha256 FROM recipes_inbox_file
             UNION SELECT json_extract(payload, '$.leadImage.sha256') FROM recipes_inbox
               WHERE json_extract(payload, '$.leadImage.sha256') IS NOT NULL
             ORDER BY 1",
        )
        .map_err(sql)?;
    let rows = statement.query_map([], |row| row.get(0)).map_err(sql)?;
    rows.collect::<Result<_, _>>().map_err(sql)
}

/// A sortable time for file names: `20261118T103700Z`.
fn stamp(ms: i64) -> String {
    iso(ms)
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(16)
        .collect()
}

/// The time in an archive's file name (`fond-<seq>-<time>.json.gz`).
fn stamp_of(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(".json.gz"))
        .and_then(|name| name.rsplit('-').next())
        .unwrap_or_default()
        .to_owned()
}

/// Writes `archive` into `dir` under its id (blocking): a temporary file,
/// synced and renamed, the directory synced, then read back and compared. A
/// damaged archive is no backup.
fn write(dir: &Path, archive: &Archive) -> R<PathBuf> {
    std::fs::create_dir_all(dir).map_err(io)?;
    let path = dir.join(format!("{}.json.gz", archive.archive_id));
    let temp = dir.join(format!(
        "{}.{}.tmp",
        archive.archive_id,
        omni_core::ids::uuid_v4()
    ));
    let text = serde_json::to_string(archive).map_err(corrupt)?;
    let mut encoder = GzEncoder::new(
        std::fs::File::create(&temp).map_err(io)?,
        Compression::default(),
    );
    encoder.write_all(text.as_bytes()).map_err(io)?;
    encoder.finish().map_err(io)?.sync_all().map_err(io)?;
    std::fs::rename(&temp, &path).map_err(io)?;
    crate::files::sync_dir(&path).map_err(io)?;
    if read(&path)? != *archive {
        return Err(archive_error("the archive did not read back as written"));
    }
    Ok(path)
}

fn build(conn: &Connection, now: i64) -> R<Archive> {
    let tables = dump(conn)?;
    let seq = db::current_seq(conn)?;
    let stamp = stamp(now);
    let mut counts = Map::new();
    let mut checksums = Map::new();
    for (table, rows) in &tables {
        counts.insert(table.clone(), json!(rows.as_array().map_or(0, Vec::len)));
        checksums.insert(table.clone(), json!(checksum(rows)));
    }
    Ok(Archive {
        format: ARCHIVE_FORMAT.to_owned(),
        version: ARCHIVE_VERSION,
        archive_id: format!("fond-{seq:010}-{stamp}"),
        created_at: iso(now),
        schema_version: SCHEMA_VERSION,
        seq,
        cook_seq: meta_u64(conn, "cook_seq")?,
        counts,
        checksums,
        image_digests: referenced_digests(conn)?,
        tables,
    })
}

/// Reads and checks an archive file: format, version and every table's
/// checksum and row count.
pub fn read(path: &Path) -> R<Archive> {
    let file = std::fs::File::open(path).map_err(io)?;
    let mut text = String::new();
    GzDecoder::new(file)
        .take(MAX_ARCHIVE_BYTES)
        .read_to_string(&mut text)
        .map_err(|e| {
            archive_error(format!(
                "{} is not a readable gzip archive: {e}",
                path.display()
            ))
        })?;
    let archive: Archive = serde_json::from_str(&text)
        .map_err(|e| archive_error(format!("{} is not a Fond archive: {e}", path.display())))?;
    if archive.format != ARCHIVE_FORMAT || archive.version != ARCHIVE_VERSION {
        return Err(archive_error(format!(
            "unsupported archive format {} version {}",
            archive.format, archive.version
        )));
    }
    if archive.schema_version > SCHEMA_VERSION {
        return Err(archive_error(format!(
            "the archive is from schema version {}; this build reads up to {SCHEMA_VERSION}",
            archive.schema_version
        )));
    }
    for table in TABLES {
        let Some(rows) = archive.tables.get(*table) else {
            if archive.schema_version < schema::table_since(table) {
                continue;
            }
            return Err(archive_error(format!("the archive has no table {table}")));
        };
        let expected = archive.checksums.get(*table).and_then(Value::as_str);
        if expected != Some(checksum(rows).as_str()) {
            return Err(archive_error(format!(
                "table {table} does not match its checksum; the archive is damaged"
            )));
        }
        let count = archive.counts.get(*table).and_then(Value::as_u64);
        if count != Some(rows.as_array().map_or(0, Vec::len) as u64) {
            return Err(archive_error(format!(
                "table {table} does not match its row count; the archive is damaged"
            )));
        }
    }
    Ok(archive)
}

/// Checks an archive and, with a recipes directory, that every image it
/// refers to is stored there with matching bytes.
pub fn verify_file(path: &Path, recipes_dir: Option<&Path>) -> R<Verified> {
    let archive = read(path)?;
    let images_checked = match recipes_dir {
        Some(dir) => {
            let files = crate::files::ImageFiles::new(dir);
            let mut checked = 0;
            for digest in &archive.image_digests {
                let stored = files
                    .path(digest, Rendition::Original)
                    .and_then(|path| std::fs::read(path).ok())
                    .ok_or_else(|| {
                        archive_error(format!("image {digest} is missing from {}", dir.display()))
                    })?;
                if sha256_hex(&stored) != *digest {
                    return Err(archive_error(format!("image {digest} is damaged")));
                }
                checked += 1;
            }
            Some(checked)
        }
        None => None,
    };
    Ok(Verified {
        archive_id: archive.archive_id,
        seq: archive.seq,
        created_at: archive.created_at,
        counts: archive.counts,
        images_checked,
    })
}

fn insert_rows(conn: &Connection, table: &str, rows: &Value) -> R<()> {
    for row in rows.as_array().into_iter().flatten() {
        let Some(object) = row.as_object() else {
            return Err(archive_error(format!("a row of {table} is not an object")));
        };
        let columns: Vec<&str> = object.keys().map(String::as_str).collect();
        if !columns.iter().all(|c| {
            c.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        }) {
            return Err(archive_error(format!(
                "a row of {table} has a bad column name"
            )));
        }
        let values: Vec<Sql> = object
            .values()
            .map(|value| match value {
                Value::Null => Sql::Null,
                Value::Bool(flag) => Sql::Integer(i64::from(*flag)),
                Value::Number(number) => match number.as_i64() {
                    Some(integer) => Sql::Integer(integer),
                    None => Sql::Real(number.as_f64().unwrap_or(0.0)),
                },
                Value::String(text) => Sql::Text(text.clone()),
                other => Sql::Text(other.to_string()),
            })
            .collect();
        let slots: Vec<String> = (1..=columns.len()).map(|n| format!("?{n}")).collect();
        conn.execute(
            &format!(
                "INSERT INTO {table} ({}) VALUES ({})",
                columns.join(", "),
                slots.join(", ")
            ),
            rusqlite::params_from_iter(values.iter()),
        )
        .map_err(sql)?;
    }
    Ok(())
}

/// What a restore did.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Restored {
    pub archive_id: String,
    pub seq: u64,
    pub counts: Map<String, Value>,
}

fn restore_tx(conn: &Connection, archive: &Archive) -> R<Restored> {
    for table in TABLES {
        let rows: i64 = conn
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .map_err(sql)?;
        if rows > 0 {
            return Err(archive_error(format!(
                "the library is not empty ({table} has {rows} rows); a restore needs an empty library"
            )));
        }
    }
    for table in TABLES {
        if let Some(rows) = archive.tables.get(*table) {
            insert_rows(conn, table, rows)?;
        }
    }
    set_meta(conn, "seq", &archive.seq.to_string())?;
    set_meta(conn, "cook_seq", &archive.cook_seq.to_string())?;
    conn.execute("DELETE FROM recipes_fts", []).map_err(sql)?;
    let mut statement = conn.prepare("SELECT id FROM recipes_recipe").map_err(sql)?;
    let ids: Vec<String> = statement
        .query_map([], |row| row.get(0))
        .map_err(sql)?
        .collect::<Result<_, _>>()
        .map_err(sql)?;
    for id in &ids {
        db::refresh_fts(conn, id)?;
    }
    // The restored tables must read back exactly as the archive holds them.
    for (table, rows) in dump(conn)? {
        let expected = match archive.checksums.get(&table).and_then(Value::as_str) {
            Some(expected) => expected.to_owned(),
            // A table newer than the archive stays empty.
            None if archive.schema_version < schema::table_since(&table) => checksum(&json!([])),
            None => String::new(),
        };
        if expected != checksum(&rows) {
            return Err(corrupt(format!(
                "restored table {table} does not match the archive; nothing was restored"
            )));
        }
    }
    // An archive from an earlier schema can carry a table that has since
    // been retired; its rows are not restored and not counted.
    let counts = archive
        .counts
        .iter()
        .filter(|(table, _)| TABLES.contains(&table.as_str()))
        .map(|(table, count)| (table.clone(), count.clone()))
        .collect();
    Ok(Restored {
        archive_id: archive.archive_id.clone(),
        seq: archive.seq,
        counts,
    })
}

/// What one backup run did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackupOutcome {
    /// Nothing has ever been written.
    Empty,
    /// The newest archive already holds this state.
    UpToDate {
        archive_id: String,
    },
    Written {
        archive_id: String,
        seq: u64,
    },
}

impl Recipes {
    /// Writes a verified archive of the library as it is now into
    /// `archives/purge`, where the hourly rotation never deletes it, and
    /// drops the ones older than [`PURGE_ARCHIVE_DAYS`]. `RecipesPurge` calls
    /// it before it removes any recipe for good.
    pub async fn archive_before_purge(&self) -> R<String> {
        self.ready().await?;
        let now = self.now();
        let mut archive = self.query(move |conn| build(conn, now)).await?;
        archive.archive_id = archive.archive_id.replacen("fond-", "fond-purge-", 1);
        let id = archive.archive_id.clone();
        let dir = self.archives_dir().join("purge");
        let oldest = stamp(now - PURGE_ARCHIVE_DAYS * 24 * 60 * 60 * 1000);
        tokio::task::spawn_blocking(move || -> R<()> {
            write(&dir, &archive)?;
            for entry in std::fs::read_dir(&dir).map_err(io)? {
                let path = entry.map_err(io)?.path();
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name.starts_with("fond-purge-")
                    && name.ends_with(".json.gz")
                    && stamp_of(&path) < oldest
                {
                    std::fs::remove_file(&path).map_err(io)?;
                }
            }
            Ok(())
        })
        .await
        .map_err(io)??;
        Ok(id)
    }

    /// The archive files, oldest first.
    pub fn archive_files(&self) -> std::io::Result<Vec<PathBuf>> {
        let mut found = Vec::new();
        let entries = match std::fs::read_dir(self.archives_dir()) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(found),
            Err(error) => return Err(error),
        };
        for entry in entries {
            let path = entry?.path();
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            if name.starts_with("fond-") && name.ends_with(".json.gz") {
                found.push(path);
            }
        }
        // By the time in the name, not by the sequence number before it: a
        // library restored from an older archive counts from a lower number.
        found.sort_by_key(|path| (stamp_of(path), path.clone()));
        Ok(found)
    }

    /// Writes an archive when the library changed since the newest one,
    /// re-reads and verifies it, and keeps the newest [`KEEP`].
    pub async fn backup(&self) -> R<BackupOutcome> {
        self.ready().await?;
        let now = self.now();
        let (state, archive) = self
            .query(move |conn| {
                let state = (db::current_seq(conn)?, meta_u64(conn, "cook_seq")?);
                let done = (
                    meta_u64(conn, "backup_seq")?,
                    meta_u64(conn, "backup_cook_seq")?,
                    schema::meta(conn, "backup_id")?,
                );
                if state == (0, 0) {
                    return Ok((state, Err(BackupOutcome::Empty)));
                }
                if let Some(id) = done.2
                    && (done.0, done.1) == state
                {
                    return Ok((state, Err(BackupOutcome::UpToDate { archive_id: id })));
                }
                Ok((state, Ok(build(conn, now)?)))
            })
            .await?;
        let archive = match archive {
            Ok(archive) => archive,
            Err(BackupOutcome::UpToDate { archive_id }) => {
                // The recorded archive must still be on disk to count.
                let path = self.archives_dir().join(format!("{archive_id}.json.gz"));
                if tokio::fs::try_exists(&path).await.unwrap_or(false) {
                    return Ok(BackupOutcome::UpToDate { archive_id });
                }
                self.query(move |conn| build(conn, now)).await?
            }
            Err(outcome) => return Ok(outcome),
        };
        let dir = self.archives_dir();
        let id = archive.archive_id.clone();
        let files = self.clone();
        tokio::task::spawn_blocking(move || -> R<()> {
            write(&dir, &archive)?;
            let all = files.archive_files().map_err(io)?;
            for old in all.iter().take(all.len().saturating_sub(KEEP)) {
                std::fs::remove_file(old).map_err(io)?;
            }
            Ok(())
        })
        .await
        .map_err(io)??;
        let recorded = id.clone();
        self.transact(false, move |conn, _| {
            set_meta(conn, "backup_seq", &state.0.to_string())?;
            set_meta(conn, "backup_cook_seq", &state.1.to_string())?;
            set_meta(conn, "backup_id", &recorded)
        })
        .await?;
        Ok(BackupOutcome::Written {
            archive_id: id,
            seq: state.0,
        })
    }

    /// Restores an archive into an empty library. Nothing is written unless
    /// the restored tables read back exactly as the archive holds them.
    pub async fn restore(&self, path: PathBuf) -> R<Restored> {
        self.ready().await?;
        let archive = tokio::task::spawn_blocking(move || read(&path))
            .await
            .map_err(io)??;
        let restored = self
            .transact(false, move |conn, _| restore_tx(conn, &archive))
            .await?;
        self.hub.library(restored.seq);
        Ok(restored)
    }

    /// Every table's rows, for comparing two libraries.
    pub async fn dump(&self) -> R<Map<String, Value>> {
        self.query(dump).await
    }
}
