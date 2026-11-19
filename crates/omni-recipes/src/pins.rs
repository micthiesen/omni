//! Pinned recipes: the ones the owner keeps at the top of the library, in an
//! order of their own. The list is stored on the server so every device and
//! every agent sees the same one.

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::json;

use crate::api::{PinInfo, PinsInput, PinsOutput};
use crate::db::{self, Actor, Change, log_change, next_seq};
use crate::error::{R, invalid, sql};
use crate::meta::PINS;
use crate::service::Recipes;

/// Recipes that can be pinned at once.
pub const MAX_PINS: usize = 60;

/// The pinned recipes that are still in the library, first to last.
pub fn order(conn: &Connection) -> R<Vec<String>> {
    let mut statement = conn
        .prepare(
            "SELECT p.recipe_id FROM recipes_pin p
             JOIN recipes_recipe r ON r.id = p.recipe_id
             WHERE r.state != 'trashed' ORDER BY p.position, p.recipe_id",
        )
        .map_err(sql)?;
    let rows = statement.query_map([], |row| row.get(0)).map_err(sql)?;
    rows.collect::<Result<_, _>>().map_err(sql)
}

/// The position stored for `id` when it is pinned and in the library. The
/// app orders pinned recipes by it. Positions can have gaps (a pinned recipe
/// went to the trash) and are never re-counted behind a recipe's back, so a
/// device that synced an earlier list still orders it correctly.
pub fn position(conn: &Connection, id: &str) -> R<Option<u32>> {
    conn.query_row(
        "SELECT p.position FROM recipes_pin p JOIN recipes_recipe r ON r.id = p.recipe_id
         WHERE p.recipe_id = ?1 AND r.state != 'trashed'",
        params![id],
        |row| row.get(0),
    )
    .optional()
    .map_err(sql)
}

/// The place of `id` among the pinned recipes, counted from 1, for the tools.
pub fn place(conn: &Connection, id: &str) -> R<Option<u32>> {
    Ok(order(conn)?
        .iter()
        .position(|pinned| pinned == id)
        .and_then(|at| u32::try_from(at + 1).ok()))
}

/// Stores `ids` as the pinned list and stamps every recipe whose stored
/// position changed with `seq`, so each device syncs it. Returns those
/// recipes.
pub(crate) fn replace(conn: &Connection, ids: &[String], seq: u64, now: i64) -> R<Vec<String>> {
    let mut statement = conn
        .prepare("SELECT recipe_id, position FROM recipes_pin")
        .map_err(sql)?;
    let before: Vec<(String, i64)> = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(sql)?
        .collect::<Result<_, _>>()
        .map_err(sql)?;
    // Ids that have left the library since (an undo of an old change) are skipped.
    let mut kept: Vec<String> = Vec::new();
    for id in ids {
        if !kept.contains(id) && db::recipe(conn, id)?.is_some_and(|row| row.state != "trashed") {
            kept.push(id.clone());
        }
    }
    conn.execute("DELETE FROM recipes_pin", []).map_err(sql)?;
    for (position, id) in kept.iter().enumerate() {
        conn.execute(
            "INSERT INTO recipes_pin (recipe_id, position) VALUES (?1, ?2)",
            params![id, i64::try_from(position).unwrap_or(i64::MAX)],
        )
        .map_err(sql)?;
    }
    let was = |id: &str| {
        before
            .iter()
            .find(|(other, _)| other == id)
            .map(|(_, at)| *at)
    };
    let is = |id: &str| {
        kept.iter()
            .position(|other| other == id)
            .and_then(|at| i64::try_from(at).ok())
    };
    let mut moved: Vec<String> = Vec::new();
    for id in before.iter().map(|(id, _)| id).chain(&kept) {
        if was(id) != is(id) && !moved.contains(id) && db::recipe(conn, id)?.is_some() {
            db::touch(conn, id, seq, now)?;
            moved.push(id.clone());
        }
    }
    Ok(moved)
}

/// Sets the pinned list to `ids`, in order, as one logged change. Returns
/// the change's id, or none when the list already was that. An agent's list
/// is refused when it names a recipe that cannot be pinned. The app's
/// (`lenient`) is taken as far as it goes: the app sends it from what it
/// holds, possibly offline and behind, and has no way to send it again
/// differently.
pub(crate) fn set(
    conn: &Connection,
    now: i64,
    ids: &[String],
    actor: &Actor,
    summary: &str,
    lenient: bool,
) -> R<Option<String>> {
    if ids.len() > MAX_PINS && !lenient {
        return Err(invalid(format!("at most {MAX_PINS} recipes can be pinned")));
    }
    let mut wanted: Vec<String> = Vec::new();
    for id in ids {
        // A recipe that was merged away is pinned as the recipe it became.
        let found = db::recipe_for_read(conn, id)
            .and_then(|(row, _)| db::live_recipe_for_write(conn, &row.id));
        let row = match found {
            Ok(row) => row,
            Err(error) if lenient && error.is_rejection() => continue,
            Err(error) => return Err(error),
        };
        if !wanted.contains(&row.id) && wanted.len() < MAX_PINS {
            wanted.push(row.id);
        }
    }
    let before = order(conn)?;
    if before == wanted {
        return Ok(None);
    }
    let seq = next_seq(conn)?;
    replace(conn, &wanted, seq, now)?;
    log_change(
        conn,
        seq,
        now,
        &Change {
            actor,
            summary: summary.to_owned(),
            kind: "meta",
            touched: json!({ format!("*{PINS}"): ["order"] }),
            before: json!({ PINS: before }),
            after: json!({ PINS: wanted }),
        },
    )
    .map(Some)
}

fn listed(conn: &Connection) -> R<Vec<PinInfo>> {
    let mut pins = Vec::new();
    for id in order(conn)? {
        if let Some(row) = db::recipe(conn, &id)? {
            pins.push(PinInfo {
                id: row.id,
                title: row.title,
                archived: row.state == "archived",
            });
        }
    }
    Ok(pins)
}

impl Recipes {
    /// Reads the pinned list, or with `ids` sets it.
    pub async fn pins(&self, input: PinsInput, actor: Actor) -> R<PinsOutput> {
        let Some(ids) = input.ids else {
            return self
                .query(|conn| {
                    Ok(PinsOutput {
                        change_id: None,
                        pins: listed(conn)?,
                    })
                })
                .await;
        };
        let dry = input.dry_run.unwrap_or(false);
        let before = self.query(db::current_seq).await?;
        let (output, seq) = self
            .transact(dry, move |conn, now| {
                let change_id = set(conn, now, &ids, &actor, "Pinned recipes set", false)?;
                Ok((
                    PinsOutput {
                        change_id: change_id.filter(|_| !dry),
                        pins: listed(conn)?,
                    },
                    db::current_seq(conn)?,
                ))
            })
            .await?;
        if !dry && seq != before {
            self.hub.library(seq);
        }
        Ok(output)
    }
}
