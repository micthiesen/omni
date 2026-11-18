//! The `recipes_*` tables beside `blobs` in the docstore, and their ordered
//! migrations keyed by `recipes_meta.schema_version` (docs/recipes.md,
//! "Storage"). Every step is idempotent, so a step interrupted by a crash
//! runs again cleanly.

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::{R, sql};

/// One migration step: the statements that take the schema to `version`.
struct Migration {
    version: u32,
    /// Columns to add first, each only when it is not there yet (SQLite has
    /// no `ADD COLUMN IF NOT EXISTS`): `(table, column, type)`.
    columns: &'static [(&'static str, &'static str, &'static str)],
    statements: &'static [&'static str],
}

const V1: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS recipes_recipe (
       id TEXT PRIMARY KEY,
       head_rev INTEGER NOT NULL,
       title TEXT NOT NULL,
       state TEXT NOT NULL,
       state_changed_at INTEGER NOT NULL,
       notes TEXT NOT NULL DEFAULT '',
       notes_rev INTEGER NOT NULL DEFAULT 0,
       cover_image TEXT,
       source_item TEXT,
       source_url TEXT,
       derived_from TEXT,
       merged_into TEXT,
       active_min INTEGER NOT NULL,
       total_min INTEGER NOT NULL,
       cook_count INTEGER NOT NULL DEFAULT 0,
       last_cooked_at INTEGER,
       created_at INTEGER NOT NULL,
       updated_at INTEGER NOT NULL,
       seq INTEGER NOT NULL)",
    "CREATE INDEX IF NOT EXISTS recipes_recipe_by_seq ON recipes_recipe (seq)",
    "CREATE INDEX IF NOT EXISTS recipes_recipe_by_source ON recipes_recipe (source_url)",
    "CREATE TABLE IF NOT EXISTS recipes_revision (
       recipe_id TEXT NOT NULL,
       rev INTEGER NOT NULL,
       format INTEGER NOT NULL,
       doc TEXT NOT NULL,
       doc_sha256 TEXT NOT NULL,
       summary TEXT NOT NULL,
       tool TEXT NOT NULL,
       call_id TEXT NOT NULL,
       created_at INTEGER NOT NULL,
       PRIMARY KEY (recipe_id, rev))",
    "CREATE INDEX IF NOT EXISTS recipes_revision_by_digest ON recipes_revision (doc_sha256)",
    // Revisions are append-only, and leave only with a recipe purged from the trash.
    "CREATE TRIGGER IF NOT EXISTS recipes_revision_no_update BEFORE UPDATE ON recipes_revision
       BEGIN SELECT RAISE(ABORT, 'recipe revisions are append-only'); END",
    "CREATE TRIGGER IF NOT EXISTS recipes_revision_keep BEFORE DELETE ON recipes_revision
       WHEN (SELECT state FROM recipes_recipe WHERE id = OLD.recipe_id) IS NOT 'trashed'
       BEGIN SELECT RAISE(ABORT, 'recipe revisions are deleted only from the trash'); END",
    "CREATE TABLE IF NOT EXISTS recipes_tag (
       name TEXT PRIMARY KEY,
       grp TEXT NOT NULL,
       description TEXT,
       created_at INTEGER NOT NULL)",
    "CREATE TABLE IF NOT EXISTS recipes_recipe_tag (
       recipe_id TEXT NOT NULL,
       tag TEXT NOT NULL,
       PRIMARY KEY (recipe_id, tag))",
    "CREATE INDEX IF NOT EXISTS recipes_recipe_tag_by_tag ON recipes_recipe_tag (tag)",
    "CREATE TABLE IF NOT EXISTS recipes_image (
       id TEXT PRIMARY KEY,
       recipe_id TEXT NOT NULL,
       sha256 TEXT NOT NULL,
       mime TEXT NOT NULL,
       width INTEGER NOT NULL,
       height INTEGER NOT NULL,
       bytes INTEGER NOT NULL,
       role TEXT NOT NULL,
       caption TEXT,
       origin TEXT NOT NULL,
       color TEXT NOT NULL,
       position INTEGER NOT NULL,
       created_at INTEGER NOT NULL)",
    "CREATE INDEX IF NOT EXISTS recipes_image_by_recipe ON recipes_image (recipe_id, position)",
    "CREATE INDEX IF NOT EXISTS recipes_image_by_digest ON recipes_image (sha256)",
    "CREATE TABLE IF NOT EXISTS recipes_inbox (
       id TEXT PRIMARY KEY,
       kind TEXT NOT NULL,
       state TEXT NOT NULL,
       title TEXT,
       payload TEXT NOT NULL,
       external_id TEXT,
       created_at INTEGER NOT NULL,
       resolved_at INTEGER,
       recipe_id TEXT)",
    "CREATE UNIQUE INDEX IF NOT EXISTS recipes_inbox_by_external
       ON recipes_inbox (external_id) WHERE external_id IS NOT NULL",
    "CREATE TABLE IF NOT EXISTS recipes_inbox_file (
       item_id TEXT NOT NULL,
       idx INTEGER NOT NULL,
       sha256 TEXT NOT NULL,
       mime TEXT NOT NULL,
       bytes INTEGER NOT NULL,
       name TEXT,
       width INTEGER NOT NULL,
       height INTEGER NOT NULL,
       color TEXT NOT NULL,
       PRIMARY KEY (item_id, idx))",
    "CREATE INDEX IF NOT EXISTS recipes_inbox_file_by_digest ON recipes_inbox_file (sha256)",
    "CREATE TABLE IF NOT EXISTS recipes_cook (
       id TEXT PRIMARY KEY,
       recipe_id TEXT NOT NULL,
       rev INTEGER NOT NULL,
       scale REAL NOT NULL,
       state TEXT NOT NULL,
       started_at INTEGER NOT NULL,
       finished_at INTEGER,
       last_event_at INTEGER NOT NULL,
       note TEXT,
       photo_image TEXT)",
    "CREATE INDEX IF NOT EXISTS recipes_cook_by_recipe ON recipes_cook (recipe_id)",
    "CREATE INDEX IF NOT EXISTS recipes_cook_by_state ON recipes_cook (state)",
    "CREATE TABLE IF NOT EXISTS recipes_cook_event (
       session_id TEXT NOT NULL,
       seq INTEGER NOT NULL,
       client_event_id TEXT NOT NULL UNIQUE,
       at INTEGER NOT NULL,
       actor TEXT NOT NULL,
       type TEXT NOT NULL,
       payload TEXT NOT NULL,
       PRIMARY KEY (session_id, seq))",
    "CREATE TRIGGER IF NOT EXISTS recipes_cook_event_no_update BEFORE UPDATE ON recipes_cook_event
       BEGIN SELECT RAISE(ABORT, 'cook events are append-only'); END",
    "CREATE TABLE IF NOT EXISTS recipes_adjustment (
       id TEXT PRIMARY KEY,
       session_id TEXT NOT NULL,
       recipe_id TEXT NOT NULL,
       target TEXT NOT NULL,
       original TEXT NOT NULL,
       text TEXT NOT NULL,
       state TEXT NOT NULL,
       resolved_rev INTEGER,
       created_at INTEGER NOT NULL)",
    "CREATE INDEX IF NOT EXISTS recipes_adjustment_by_recipe ON recipes_adjustment (recipe_id, state)",
    "CREATE TABLE IF NOT EXISTS recipes_change (
       id TEXT PRIMARY KEY,
       seq INTEGER NOT NULL,
       at INTEGER NOT NULL,
       tool TEXT NOT NULL,
       call_id TEXT NOT NULL,
       summary TEXT NOT NULL,
       kind TEXT NOT NULL,
       touched TEXT NOT NULL,
       before TEXT NOT NULL,
       after TEXT NOT NULL,
       undone_by TEXT)",
    "CREATE INDEX IF NOT EXISTS recipes_change_by_seq ON recipes_change (seq)",
    "CREATE TABLE IF NOT EXISTS recipes_client_op (
       id TEXT PRIMARY KEY,
       applied_at INTEGER NOT NULL,
       result TEXT NOT NULL)",
    // Where a purged, merged-away recipe now lives.
    "CREATE TABLE IF NOT EXISTS recipes_redirect (
       id TEXT PRIMARY KEY,
       merged_into TEXT NOT NULL,
       purged_at INTEGER NOT NULL)",
    // One row per ended timer: the durable reservation of its alert.
    "CREATE TABLE IF NOT EXISTS recipes_timer_alert (
       session_id TEXT NOT NULL,
       step TEXT NOT NULL,
       ends_at INTEGER NOT NULL,
       state TEXT NOT NULL,
       at INTEGER NOT NULL,
       PRIMARY KEY (session_id, step, ends_at))",
    "CREATE VIRTUAL TABLE IF NOT EXISTS recipes_fts USING fts5(
       recipe_id UNINDEXED, title, ingredients, tags, body, notes, source, folded,
       tokenize = \"unicode61 remove_diacritics 2\", prefix = '2 3 4')",
];

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        statements: V1,
        columns: &[],
    },
    // A cook session remembers which recipe's revision it pinned, so a merge
    // can move the session without changing what it shows.
    Migration {
        version: 2,
        statements: &[
            "UPDATE recipes_cook SET doc_recipe_id = recipe_id WHERE doc_recipe_id IS NULL",
        ],
        columns: &[("recipes_cook", "doc_recipe_id", "TEXT")],
    },
];

/// The newest schema version this build writes.
pub const SCHEMA_VERSION: u32 = 2;

/// Every table an archive carries, and a restore compares.
pub const TABLES: &[&str] = &[
    "recipes_recipe",
    "recipes_revision",
    "recipes_tag",
    "recipes_recipe_tag",
    "recipes_image",
    "recipes_inbox",
    "recipes_inbox_file",
    "recipes_cook",
    "recipes_cook_event",
    "recipes_adjustment",
    "recipes_change",
    "recipes_client_op",
    "recipes_redirect",
];

pub fn meta(conn: &Connection, key: &str) -> R<Option<String>> {
    conn.query_row(
        "SELECT value FROM recipes_meta WHERE key = ?1",
        params![key],
        |row| row.get(0),
    )
    .optional()
    .map_err(sql)
}

pub fn set_meta(conn: &Connection, key: &str, value: &str) -> R<()> {
    conn.execute(
        "INSERT INTO recipes_meta (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )
    .map_err(sql)?;
    Ok(())
}

pub fn meta_u64(conn: &Connection, key: &str) -> R<u64> {
    Ok(meta(conn, key)?
        .and_then(|value| value.parse().ok())
        .unwrap_or(0))
}

/// Creates or upgrades the tables. A database written by a newer build is
/// refused, so an old binary never writes rows it does not understand.
pub fn migrate(conn: &Connection) -> R<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS recipes_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
    )
    .map_err(sql)?;
    let current = u32::try_from(meta_u64(conn, "schema_version")?).unwrap_or(u32::MAX);
    if current > SCHEMA_VERSION {
        return Err(crate::error::corrupt(format!(
            "the recipe tables are at schema version {current}; this build reads up to {SCHEMA_VERSION}"
        )));
    }
    for migration in MIGRATIONS.iter().filter(|m| m.version > current) {
        for (table, column, kind) in migration.columns {
            let present: i64 = conn
                .query_row(
                    "SELECT count(*) FROM pragma_table_info(?1) WHERE name = ?2",
                    params![table, column],
                    |row| row.get(0),
                )
                .map_err(sql)?;
            if present == 0 {
                conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {kind}"))
                    .map_err(sql)?;
            }
        }
        for statement in migration.statements {
            conn.execute_batch(statement).map_err(sql)?;
        }
        set_meta(conn, "schema_version", &migration.version.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn open() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn
    }

    #[test]
    fn migrations_are_idempotent_and_record_the_version() {
        let conn = open();
        migrate(&conn).unwrap();
        assert_eq!(
            meta_u64(&conn, "schema_version").unwrap(),
            u64::from(SCHEMA_VERSION)
        );
        // A step interrupted before its version was recorded runs again.
        set_meta(&conn, "schema_version", "0").unwrap();
        migrate(&conn).unwrap();
        assert_eq!(
            meta_u64(&conn, "schema_version").unwrap(),
            u64::from(SCHEMA_VERSION)
        );
    }

    #[test]
    fn the_second_step_upgrades_a_first_version_database_in_place() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE recipes_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)")
            .unwrap();
        for statement in V1 {
            conn.execute_batch(statement).unwrap();
        }
        set_meta(&conn, "schema_version", "1").unwrap();
        conn.execute(
            "INSERT INTO recipes_cook (id, recipe_id, rev, scale, state, started_at, last_event_at)
             VALUES ('cook_a', 'rcp_a', 1, 1.0, 'active', 0, 0)",
            [],
        )
        .unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap();
        let pinned: String = conn
            .query_row("SELECT doc_recipe_id FROM recipes_cook", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(pinned, "rcp_a");
    }

    #[test]
    fn a_newer_schema_is_refused() {
        let conn = open();
        set_meta(&conn, "schema_version", "99").unwrap();
        assert!(migrate(&conn).is_err());
    }

    #[test]
    fn the_bundled_sqlite_has_fts5_with_prefix_and_diacritic_folding() {
        let conn = open();
        conn.execute(
            "INSERT INTO recipes_fts (recipe_id, title, ingredients, tags, body, notes, source, folded)
             VALUES ('rcp_a', 'Crème Brûlée', 'cream', '', '', '', '', '')",
            [],
        )
        .unwrap();
        for query in ["\"creme\"*", "\"brul\"*", "title : \"creme\""] {
            let found: i64 = conn
                .query_row(
                    "SELECT count(*) FROM recipes_fts WHERE recipes_fts MATCH ?1",
                    params![query],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(found, 1, "{query}");
        }
    }

    #[test]
    fn revisions_are_append_only_outside_the_trash() {
        let conn = open();
        conn.execute(
            "INSERT INTO recipes_recipe (id, head_rev, title, state, state_changed_at, active_min,
               total_min, created_at, updated_at, seq) VALUES ('rcp_a', 1, 'A', 'active', 0, 0, 0, 0, 0, 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO recipes_revision VALUES ('rcp_a', 1, 1, '{}', 'x', 's', 't', 'c', 0)",
            [],
        )
        .unwrap();
        assert!(
            conn.execute("UPDATE recipes_revision SET summary = 'y'", [])
                .is_err()
        );
        assert!(conn.execute("DELETE FROM recipes_revision", []).is_err());
        conn.execute("UPDATE recipes_recipe SET state = 'trashed'", [])
            .unwrap();
        assert_eq!(conn.execute("DELETE FROM recipes_revision", []).unwrap(), 1);
    }
}
