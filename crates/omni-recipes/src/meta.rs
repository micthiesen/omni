//! Snapshots of record metadata, the unit the change log stores and
//! `recipes_undo` replays.
//!
//! A snapshot holds, per recipe, only the facets a change writes (`state`,
//! `tags`, `notes`, `images`, `cooks`), and optionally the whole tag registry
//! and which recipe owns which cook sessions. Applying a
//! snapshot puts exactly those things back, so a change's `before` undoes it
//! and its `after` redoes it.

use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::db::{self, ImageRow, images_of, refresh_fts, tags_of};
use crate::error::{R, corrupt, not_found, sql};

pub const STATE: &str = "state";
pub const TAGS: &str = "tags";
pub const NOTES: &str = "notes";
pub const IMAGES: &str = "images";
pub const COOKS: &str = "cooks";
/// The key of the tag registry in a change's `touched` map.
pub const TAG_KEY_PREFIX: &str = "tag:";
/// The facet of a tag a change put on or took off recipes.
pub const USE: &str = "use";
pub const REGISTRY: &str = "registry";
/// The key of the pinned list in a change's `touched` map and in a snapshot.
pub const PINS: &str = "pins";
/// The key under which a snapshot holds the state of inbox items.
pub const INBOX: &str = "inbox";

/// The state of each listed inbox item: `{id: {state, resolvedAt}}`.
pub fn inbox_states(conn: &Connection, items: &[String]) -> R<Value> {
    let mut out = Map::new();
    for id in items {
        let found: Option<(String, Option<i64>)> =
            rusqlite::OptionalExtension::optional(conn.query_row(
                "SELECT state, resolved_at FROM recipes_inbox WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            ))
            .map_err(sql)?;
        if let Some((state, resolved_at)) = found {
            out.insert(
                id.clone(),
                json!({ "state": state, "resolvedAt": resolved_at }),
            );
        }
    }
    Ok(Value::Object(out))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TagRow {
    pub name: String,
    pub grp: String,
    pub description: Option<String>,
    pub created_at: i64,
}

pub fn registry_rows(conn: &Connection) -> R<Vec<TagRow>> {
    let mut statement = conn
        .prepare("SELECT name, grp, description, created_at FROM recipes_tag ORDER BY name")
        .map_err(sql)?;
    let rows = statement
        .query_map([], |row| {
            Ok(TagRow {
                name: row.get(0)?,
                grp: row.get(1)?,
                description: row.get(2)?,
                created_at: row.get(3)?,
            })
        })
        .map_err(sql)?;
    rows.collect::<Result<_, _>>().map_err(sql)
}

/// The named facets of one recipe.
pub fn recipe_facets(conn: &Connection, id: &str, facets: &[&str]) -> R<Value> {
    let row = db::recipe(conn, id)?.ok_or_else(|| not_found(format!("no recipe has id {id:?}")))?;
    let mut out = Map::new();
    for facet in facets {
        let value = match *facet {
            STATE => json!({
                "state": row.state,
                "changedAt": row.state_changed_at,
                "mergedInto": row.merged_into,
                "sourceItem": row.source_item,
            }),
            TAGS => json!(tags_of(conn, id)?),
            NOTES => json!({ "text": row.notes }),
            IMAGES => json!({ "cover": row.cover_image, "rows": images_of(conn, id)? }),
            COOKS => json!({ "count": row.cook_count, "lastCookedAt": row.last_cooked_at }),
            _ => continue,
        };
        out.insert((*facet).to_owned(), value);
    }
    Ok(Value::Object(out))
}

/// `{id: facets}` for several recipes.
pub fn recipes_facets(conn: &Connection, ids: &[String], facets: &[&str]) -> R<Value> {
    let mut out = Map::new();
    for id in ids {
        out.insert(id.clone(), recipe_facets(conn, id, facets)?);
    }
    Ok(Value::Object(out))
}

/// Which recipe owns each of the listed cook sessions.
pub fn ownership(conn: &Connection, cooks: &[String]) -> R<Value> {
    let mut cook_map = Map::new();
    for id in cooks {
        let owner: Option<String> = rusqlite::OptionalExtension::optional(conn.query_row(
            "SELECT recipe_id FROM recipes_cook WHERE id = ?1",
            params![id],
            |row| row.get(0),
        ))
        .map_err(sql)?;
        if let Some(recipe) = owner {
            cook_map.insert(id.clone(), json!(recipe));
        }
    }
    Ok(json!({ "cooks": cook_map }))
}

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// Puts back everything `snapshot` holds and returns the ids of the recipes
/// it wrote. Each of them is stamped with `seq`.
pub fn apply(conn: &Connection, snapshot: &Value, seq: u64, now: i64) -> R<Vec<String>> {
    // Only the registry rows the snapshot names: a tag registered since
    // stays registered.
    if let Some(registry) = snapshot.get(REGISTRY).and_then(Value::as_object) {
        for (name, row) in registry {
            conn.execute("DELETE FROM recipes_tag WHERE name = ?1", params![name])
                .map_err(sql)?;
            if !row.is_null() {
                let row: TagRow = serde_json::from_value(row.clone()).map_err(corrupt)?;
                conn.execute(
                    "INSERT INTO recipes_tag (name, grp, description, created_at) VALUES (?1, ?2, ?3, ?4)",
                    params![row.name, row.grp, row.description, row.created_at],
                )
                .map_err(sql)?;
            }
        }
    }
    // A stored source set aside with a recipe's link to it comes back with it.
    if let Some(items) = snapshot.get(INBOX).and_then(Value::as_object) {
        for (id, state) in items {
            conn.execute(
                "UPDATE recipes_inbox SET state = ?2, resolved_at = ?3 WHERE id = ?1",
                params![
                    id,
                    text(state, "state").unwrap_or("imported"),
                    state.get("resolvedAt").and_then(Value::as_i64)
                ],
            )
            .map_err(sql)?;
        }
    }
    let mut written = Vec::new();
    if let Some(pins) = snapshot.get(PINS) {
        let ids: Vec<String> = serde_json::from_value(pins.clone()).map_err(corrupt)?;
        written.extend(crate::pins::replace(conn, &ids, seq, now)?);
    }
    let empty = Map::new();
    let recipes = snapshot
        .get("recipes")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    // Image rows move between recipes in a merge: clear every recipe's rows
    // before any are put back, so no id is inserted twice.
    for (id, facets) in recipes {
        if facets.get(IMAGES).is_some() {
            conn.execute(
                "DELETE FROM recipes_image WHERE recipe_id = ?1",
                params![id],
            )
            .map_err(sql)?;
        }
    }
    for (id, facets) in recipes {
        if db::recipe(conn, id)?.is_none() {
            return Err(not_found(format!("{id} no longer exists")));
        }
        if let Some(state) = facets.get(STATE) {
            conn.execute(
                "UPDATE recipes_recipe SET state = ?2, state_changed_at = ?3, merged_into = ?4
                 WHERE id = ?1",
                params![
                    id,
                    text(state, "state").unwrap_or("active"),
                    now,
                    text(state, "mergedInto")
                ],
            )
            .map_err(sql)?;
            // A merge can hand the kept recipe its source; undo takes it back.
            if let Some(source) = state.get("sourceItem") {
                conn.execute(
                    "UPDATE recipes_recipe SET source_item = ?2 WHERE id = ?1",
                    params![id, source.as_str()],
                )
                .map_err(sql)?;
            }
        }
        if let Some(tags) = facets.get(TAGS) {
            let tags: Vec<String> = serde_json::from_value(tags.clone()).map_err(corrupt)?;
            db::set_tags(conn, id, &tags)?;
        }
        if let Some(notes) = facets.get(NOTES) {
            conn.execute(
                "UPDATE recipes_recipe SET notes = ?2, notes_rev = notes_rev + 1 WHERE id = ?1",
                params![id, text(notes, "text").unwrap_or_default()],
            )
            .map_err(sql)?;
        }
        if let Some(images) = facets.get(IMAGES) {
            let rows: Vec<ImageRow> =
                serde_json::from_value(images.get("rows").cloned().unwrap_or_else(|| json!([])))
                    .map_err(corrupt)?;
            for row in &rows {
                db::insert_image(conn, row)?;
            }
            conn.execute(
                "UPDATE recipes_recipe SET cover_image = ?2 WHERE id = ?1",
                params![id, text(images, "cover")],
            )
            .map_err(sql)?;
        }
        if let Some(cooks) = facets.get(COOKS) {
            conn.execute(
                "UPDATE recipes_recipe SET cook_count = ?2, last_cooked_at = ?3 WHERE id = ?1",
                params![
                    id,
                    cooks.get("count").and_then(Value::as_i64).unwrap_or(0),
                    cooks.get("lastCookedAt").and_then(Value::as_i64)
                ],
            )
            .map_err(sql)?;
        }
        db::touch(conn, id, seq, now)?;
        refresh_fts(conn, id)?;
        written.push(id.clone());
    }
    // A snapshot written before adjustment records were retired also lists
    // their owners; there is nothing left to move for those.
    let cooks = snapshot
        .get("own")
        .and_then(|own| own.get("cooks"))
        .and_then(Value::as_object);
    for (id, recipe) in cooks.into_iter().flatten() {
        conn.execute(
            "UPDATE recipes_cook SET recipe_id = ?2 WHERE id = ?1",
            params![id, recipe.as_str()],
        )
        .map_err(sql)?;
    }
    Ok(written)
}

/// Every image digest a stored snapshot mentions, so the purge keeps the
/// files an undo could still need.
pub fn digests(snapshot: &Value, into: &mut std::collections::HashSet<String>) {
    let Some(recipes) = snapshot.get("recipes").and_then(Value::as_object) else {
        return;
    };
    for facets in recipes.values() {
        let rows = facets
            .get(IMAGES)
            .and_then(|images| images.get("rows"))
            .and_then(Value::as_array);
        for row in rows.into_iter().flatten() {
            if let Some(digest) = row.get("sha256").and_then(Value::as_str) {
                into.insert(digest.to_owned());
            }
        }
    }
}

/// The current state of everything `shape` (an earlier snapshot) covers.
pub fn like(conn: &Connection, shape: &Value) -> R<Value> {
    let mut out = Map::new();
    if let Some(registry) = shape.get(REGISTRY).and_then(Value::as_object) {
        let rows = registry_rows(conn)?;
        let mut map = Map::new();
        for name in registry.keys() {
            map.insert(
                name.clone(),
                json!(rows.iter().find(|row| row.name == *name)),
            );
        }
        out.insert(REGISTRY.to_owned(), Value::Object(map));
    }
    if let Some(recipes) = shape.get("recipes").and_then(Value::as_object) {
        let mut map = Map::new();
        for (id, facets) in recipes {
            let names: Vec<&str> = facets
                .as_object()
                .map(|facets| facets.keys().map(String::as_str).collect())
                .unwrap_or_default();
            map.insert(id.clone(), recipe_facets(conn, id, &names)?);
        }
        out.insert("recipes".to_owned(), Value::Object(map));
    }
    if shape.get(PINS).is_some() {
        out.insert(PINS.to_owned(), json!(crate::pins::order(conn)?));
    }
    if let Some(items) = shape.get(INBOX).and_then(Value::as_object) {
        let ids: Vec<String> = items.keys().cloned().collect();
        out.insert(INBOX.to_owned(), inbox_states(conn, &ids)?);
    }
    if let Some(own) = shape.get("own") {
        let cooks: Vec<String> = own
            .get("cooks")
            .and_then(Value::as_object)
            .map(|map| map.keys().cloned().collect())
            .unwrap_or_default();
        out.insert("own".to_owned(), ownership(conn, &cooks)?);
    }
    Ok(Value::Object(out))
}
