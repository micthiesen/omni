//! What the app reads and writes besides cook sessions: the library sync,
//! its own record edits (notes and state, each applied once), the stored
//! original of a recipe and the inbox list.

use omni_api::fond::{
    ClientOp, ClientOpKind, ImageInfo, InboxItem, InboxList, OpResult, OpsResponse, Original,
    RecipeRecord, RecipeState, SourceRef, SyncResponse, Tag,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::json;

use crate::cook;
use crate::db::{
    self, Actor, CURRENT_FORMAT, Change, RECIPE_COLUMNS, RecipeRow, cover_of, doc_at, images_of,
    log_change, next_seq, recipe_for_read, recipe_from_row, refresh_fts, tags_of,
};
use crate::error::{R, corrupt, not_found, sql};
use crate::inbox;
use crate::meta::{self, NOTES, STATE};
use crate::organize::set_state;
use crate::service::Recipes;
use crate::tags;

/// Characters the notes field holds.
pub const MAX_NOTES: usize = 20_000;

fn wire_state(row: &RecipeRow) -> RecipeState {
    match row.state.as_str() {
        "archived" => RecipeState::Archived,
        "trashed" => RecipeState::Trashed,
        _ => RecipeState::Active,
    }
}

/// One recipe as the app stores it.
pub fn record(conn: &Connection, row: &RecipeRow) -> R<RecipeRecord> {
    let doc = doc_at(conn, &row.id, row.head_rev)?;
    let images = images_of(conn, &row.id)?;
    let source = match &row.source_item {
        Some(item) => inbox::item(conn, item)?.map(|item| {
            Ok::<_, crate::error::RecipesError>(SourceRef {
                files: inbox::file_count(conn, &item.id)?,
                url: item.url(),
                kind: item.kind.clone(),
                item_id: item.id,
            })
        }),
        None => None,
    }
    .transpose()?;
    Ok(RecipeRecord {
        id: row.id.clone(),
        revision: row.head_rev,
        format: CURRENT_FORMAT,
        state: wire_state(row),
        doc: serde_json::to_value(&doc).map_err(corrupt)?,
        tags: tags_of(conn, &row.id)?,
        notes: row.notes.clone(),
        notes_rev: row.notes_rev,
        cover: cover_of(row, &images).map(|image| image.id.clone()),
        images: images
            .into_iter()
            .map(|image| ImageInfo {
                id: image.id,
                digest: image.sha256,
                width: image.width,
                height: image.height,
                color: image.color,
                role: image.role,
                caption: image.caption,
            })
            .collect(),
        source,
        active_min: row.active_min,
        total_min: row.total_min,
        cook_count: row.cook_count,
        last_cooked_at: row.last_cooked_at,
        created_at: row.created_at,
        updated_at: row.updated_at,
        seq: row.seq,
        adjustments: Vec::new(),
    })
}

fn sync(conn: &Connection, since: Option<u64>, build: Option<String>) -> R<SyncResponse> {
    let seq = db::current_seq(conn)?;
    // A sequence number from the future means another database: start over.
    let since = since.filter(|since| *since <= seq);
    let mut statement = conn
        .prepare(&format!(
            "SELECT {RECIPE_COLUMNS} FROM recipes_recipe WHERE seq > ?1 ORDER BY seq, id"
        ))
        .map_err(sql)?;
    let rows = statement
        .query_map(params![db::sql_seq(since.unwrap_or(0))], recipe_from_row)
        .map_err(sql)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql)?;
    let mut recipes = Vec::new();
    let mut removed = Vec::new();
    for row in rows {
        if row.state == "trashed" {
            if since.is_some() {
                removed.push(row.id);
            }
        } else {
            recipes.push(record(conn, &row)?);
        }
    }
    let live_count: u32 = conn
        .query_row(
            "SELECT count(*) FROM recipes_recipe WHERE state != 'trashed'",
            [],
            |row| row.get(0),
        )
        .map_err(sql)?;
    Ok(SyncResponse {
        seq,
        full: since.is_none(),
        build,
        live_count,
        recipes,
        removed,
        tags: tags::registry(conn)?
            .into_iter()
            .map(|tag| Tag {
                name: tag.name,
                group: tag.group.as_str().to_owned(),
                description: tag.description,
                count: tag.count,
            })
            .collect(),
        sessions: cook::open_sessions(conn)?,
        inbox_pending: inbox::pending_count(conn)?,
    })
}

/// Applies one op and returns `(ok, error, recipe id)`.
fn apply_op(
    conn: &Connection,
    now: i64,
    op: &ClientOp,
) -> R<(bool, Option<String>, Option<String>)> {
    let refuse = |message: &str| Ok((false, Some(message.to_owned()), None));
    let target = match &op.kind {
        ClientOpKind::Notes { recipe_id, .. }
        | ClientOpKind::State { recipe_id, .. }
        | ClientOpKind::Amount { recipe_id, .. } => recipe_id,
    };
    // An op on a recipe that was merged away lands on the recipe it became.
    let Ok((row, redirected)) = recipe_for_read(conn, target) else {
        return refuse("no such recipe");
    };
    // A note follows the recipe to where it lives now. A state change does
    // not: archiving or deleting the absorbed copy must never hit the kept one.
    if redirected.is_some() && !matches!(op.kind, ClientOpKind::Notes { .. }) {
        return refuse("[merged] the recipe was merged into another");
    }
    let ids = [row.id.clone()];
    let actor = Actor::app();
    match &op.kind {
        ClientOpKind::Notes { text, base_rev, .. } => {
            if text.chars().count() > MAX_NOTES {
                return refuse("notes are limited to 20,000 characters");
            }
            // A stale edit never overwrites: both texts are kept, the server's first.
            let fresh = *base_rev == row.notes_rev || *text == row.notes;
            let merged = if fresh || row.notes.trim().is_empty() {
                text.clone()
            } else if text.trim().is_empty() || row.notes.contains(text.trim()) {
                row.notes.clone()
            } else if text.contains(row.notes.trim()) {
                // The stale edit already holds the server's text: no repeat.
                text.clone()
            } else {
                format!("{}\n\n{}", row.notes.trim_end(), text)
            };
            // Nothing is ever cut: a merge that cannot fit leaves the
            // server's text alone and tells the app.
            if merged.chars().count() > MAX_NOTES {
                return refuse(
                    "notes are limited to 20,000 characters; this edit and the newer notes do not fit together",
                );
            }
            if merged == row.notes {
                return Ok((true, None, Some(row.id)));
            }
            let before = json!({ "recipes": meta::recipes_facets(conn, &ids, &[NOTES])? });
            let seq = next_seq(conn)?;
            conn.execute(
                "UPDATE recipes_recipe SET notes = ?2, notes_rev = notes_rev + 1, updated_at = ?3,
                   seq = ?4 WHERE id = ?1",
                params![row.id, merged, now, db::sql_seq(seq)],
            )
            .map_err(sql)?;
            refresh_fts(conn, &row.id)?;
            let after = json!({ "recipes": meta::recipes_facets(conn, &ids, &[NOTES])? });
            log_change(
                conn,
                seq,
                now,
                &Change {
                    actor: &actor,
                    summary: format!("Notes of {} edited in the app", row.title),
                    kind: "meta",
                    touched: json!({ row.id.clone(): [NOTES] }),
                    before,
                    after,
                },
            )?;
        }
        ClientOpKind::Amount {
            ingredient_id,
            qty,
            unit,
            base_revision,
            ..
        } => {
            // A refusal answers with the recipe as it is, so the app can put
            // back what it showed.
            if let Err(message) = crate::library::app_amount(
                conn,
                now,
                &row,
                ingredient_id,
                *qty,
                unit.as_deref(),
                *base_revision,
            )? {
                return Ok((false, Some(message), Some(row.id)));
            }
        }
        ClientOpKind::State { state, .. } => {
            let wanted = match state {
                RecipeState::Active => "active",
                RecipeState::Archived => "archived",
                RecipeState::Trashed => "trashed",
            };
            if row.merged_into.is_some() {
                return refuse("the recipe was merged into another");
            }
            if row.state == wanted {
                return Ok((true, None, Some(row.id)));
            }
            let before = json!({ "recipes": meta::recipes_facets(conn, &ids, &[STATE])? });
            let seq = next_seq(conn)?;
            set_state(conn, &row.id, wanted, now)?;
            db::touch(conn, &row.id, seq, now)?;
            let after = json!({ "recipes": meta::recipes_facets(conn, &ids, &[STATE])? });
            log_change(
                conn,
                seq,
                now,
                &Change {
                    actor: &actor,
                    summary: format!("{} set to {wanted} in the app", row.title),
                    kind: "meta",
                    touched: json!({ row.id.clone(): [STATE] }),
                    before,
                    after,
                },
            )?;
        }
    }
    Ok((true, None, Some(row.id)))
}

fn apply_ops(conn: &Connection, now: i64, ops: &[ClientOp]) -> R<OpsResponse> {
    let mut results = Vec::with_capacity(ops.len());
    for op in ops {
        let stored: Option<String> = conn
            .query_row(
                "SELECT result FROM recipes_client_op WHERE id = ?1",
                params![op.id],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql)?;
        let (ok, error, recipe_id) = match stored {
            // Applied before: answer as then, with the recipe as it is now.
            Some(result) => {
                let value: serde_json::Value = serde_json::from_str(&result).unwrap_or_default();
                (
                    value["ok"].as_bool().unwrap_or(false),
                    value["error"].as_str().map(str::to_owned),
                    value["recipe"].as_str().map(str::to_owned),
                )
            }
            None => {
                let outcome = apply_op(conn, now, op)?;
                conn.execute(
                    "INSERT INTO recipes_client_op (id, applied_at, result) VALUES (?1, ?2, ?3)",
                    params![
                        op.id,
                        now,
                        json!({ "ok": outcome.0, "error": outcome.1, "recipe": outcome.2 })
                            .to_string()
                    ],
                )
                .map_err(sql)?;
                outcome
            }
        };
        let recipe = match recipe_id
            .as_deref()
            .map(|id| db::recipe(conn, id))
            .transpose()?
        {
            Some(Some(row)) if row.state != "trashed" => Some(record(conn, &row)?),
            _ => None,
        };
        results.push(OpResult {
            id: op.id.clone(),
            ok,
            error,
            recipe,
        });
    }
    Ok(OpsResponse {
        seq: db::current_seq(conn)?,
        results,
    })
}

impl Recipes {
    pub async fn sync(&self, since: Option<u64>, build: Option<String>) -> R<SyncResponse> {
        self.query(move |conn| sync(conn, since, build)).await
    }

    /// Applies the app's record edits, each id once.
    pub async fn apply_ops(&self, ops: Vec<ClientOp>) -> R<OpsResponse> {
        let before = self.query(db::current_seq).await?;
        let response = self
            .transact(false, move |conn, now| apply_ops(conn, now, &ops))
            .await?;
        if response.seq != before {
            self.hub.library(response.seq);
        }
        Ok(response)
    }

    /// The stored source of a recipe.
    pub async fn original(&self, recipe_id: &str) -> R<Original> {
        let id = recipe_id.to_owned();
        self.query(move |conn| {
            let (row, _) = recipe_for_read(conn, &id)?;
            let item = match &row.source_item {
                Some(item) => inbox::item(conn, item)?,
                None => None,
            }
            .ok_or_else(|| not_found(format!("{} has no stored source", row.id)))?;
            Ok(Original {
                files: inbox::file_count(conn, &item.id)?,
                url: item.url(),
                text: item.text().map(str::to_owned),
                title: item.title.clone(),
                added_at: item.created_at,
                kind: item.kind,
                item_id: item.id,
            })
        })
        .await
    }

    /// The newest inbox items, pending first.
    pub async fn inbox_for_app(&self) -> R<InboxList> {
        self.query(|conn| {
            let mut statement = conn
                .prepare(
                    "SELECT id, kind, title, created_at, state, recipe_id FROM recipes_inbox
                     WHERE state != 'uploading'
                     ORDER BY state != 'pending', created_at DESC, id LIMIT 100",
                )
                .map_err(sql)?;
            let rows = statement
                .query_map([], |row| {
                    Ok(InboxItem {
                        id: row.get(0)?,
                        kind: row.get(1)?,
                        title: row.get(2)?,
                        added_at: row.get(3)?,
                        files: 0,
                        state: row.get(4)?,
                        recipe_id: row.get(5)?,
                    })
                })
                .map_err(sql)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql)?;
            let mut items = Vec::with_capacity(rows.len());
            for mut item in rows {
                item.files = inbox::file_count(conn, &item.id)?;
                items.push(item);
            }
            Ok(InboxList { items })
        })
        .await
    }
}
