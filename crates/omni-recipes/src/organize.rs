//! Record metadata in bulk: tags and state on listed recipes, merging
//! duplicates, and undoing a logged change.

use rusqlite::{Connection, params};
use serde_json::{Map, Value, json};

use crate::api::{
    MergeInput, MergeMoved, MergeNotes, MergeOutput, OrganizeInput, OrganizeOutput, OrganizeResult,
    UndoInput, UndoOutput,
};
use crate::db::{
    self, Actor, Change, doc_at, images_of, live_recipe_for_write, log_change, next_seq,
    recipe_for_write, refresh_fts, tags_of,
};
use crate::error::{R, invalid, not_found, reject, sql};
use crate::library::{state_of, write_revision};
use crate::meta::{self, COOKS, IMAGES, NOTES, REGISTRY, STATE, TAGS};
use crate::service::Recipes;
use crate::tags;

/// How long a change can be undone, and how long the trash is kept.
pub const RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1000;

pub(crate) fn set_state(conn: &Connection, id: &str, state: &str, now: i64) -> R<()> {
    conn.execute(
        "UPDATE recipes_recipe SET state = ?2, state_changed_at = ?3 WHERE id = ?1",
        params![id, state, now],
    )
    .map_err(sql)?;
    Ok(())
}

fn organize_tx(
    conn: &Connection,
    now: i64,
    input: &OrganizeInput,
    actor: &Actor,
) -> R<(Option<String>, u64, OrganizeOutput)> {
    let add = tags::names(input.add_tags.as_deref().unwrap_or_default())?;
    let remove = tags::names(input.remove_tags.as_deref().unwrap_or_default())?;
    if add.is_empty() && remove.is_empty() && input.state.is_none() {
        return Err(invalid("pass addTags, removeTags or state"));
    }
    if let Some(both) = add.iter().find(|tag| remove.contains(tag)) {
        return Err(invalid(format!(
            "tag {both:?} is in both addTags and removeTags"
        )));
    }
    let created = tags::require(conn, &add, input.create_tags.unwrap_or(false), now)?;
    let mut ids: Vec<String> = Vec::new();
    for id in &input.ids {
        let row = recipe_for_write(conn, id)?;
        if !ids.contains(&row.id) {
            ids.push(row.id);
        }
    }
    let mut facets: Vec<&str> = Vec::new();
    if !add.is_empty() || !remove.is_empty() {
        facets.push(TAGS);
    }
    if input.state.is_some() {
        facets.push(STATE);
    }
    // Decide every recipe's new tags and state before writing any.
    let mut plans: Vec<(String, Vec<String>, String, bool)> = Vec::new();
    for id in &ids {
        let row =
            db::recipe(conn, id)?.ok_or_else(|| not_found(format!("no recipe has id {id:?}")))?;
        let current = tags_of(conn, id)?;
        let mut next: Vec<String> = current
            .iter()
            .filter(|tag| !remove.contains(tag))
            .cloned()
            .collect();
        for tag in &add {
            if !next.contains(tag) {
                next.push(tag.clone());
            }
        }
        next.sort();
        let state = input
            .state
            .map_or(row.state.clone(), |s| s.as_str().to_owned());
        let changed = next != current || state != row.state;
        plans.push((id.clone(), next, state, changed));
    }
    let changed_ids: Vec<String> = plans
        .iter()
        .filter(|plan| plan.3)
        .map(|plan| plan.0.clone())
        .collect();
    let mut change_id = None;
    let mut seq = db::current_seq(conn)?;
    if !changed_ids.is_empty() {
        let before = json!({ "recipes": meta::recipes_facets(conn, &changed_ids, &facets)? });
        seq = next_seq(conn)?;
        let mut touched = Map::new();
        for (id, next, state, changed) in &plans {
            if !changed {
                continue;
            }
            if facets.contains(&TAGS) {
                db::set_tags(conn, id, next)?;
            }
            if facets.contains(&STATE) {
                set_state(conn, id, state, now)?;
            }
            db::touch(conn, id, seq, now)?;
            refresh_fts(conn, id)?;
            touched.insert(id.clone(), json!(facets));
        }
        let used: Vec<String> = add.iter().chain(&remove).cloned().collect();
        tags::touch(&mut touched, &used, &created);
        let after = json!({ "recipes": meta::recipes_facets(conn, &changed_ids, &facets)? });
        let summary = input
            .summary
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| format!("Organize {} recipe(s)", changed_ids.len()));
        change_id = Some(log_change(
            conn,
            seq,
            now,
            &Change {
                actor,
                summary,
                kind: "meta",
                touched: Value::Object(touched),
                before,
                after,
            },
        )?);
    }
    let mut results = Vec::new();
    for id in &ids {
        let row =
            db::recipe(conn, id)?.ok_or_else(|| not_found(format!("no recipe has id {id:?}")))?;
        results.push(OrganizeResult {
            tags: tags_of(conn, id)?,
            state: state_of(&row),
            title: row.title,
            id: row.id,
        });
    }
    let changed = u32::try_from(changed_ids.len()).unwrap_or(u32::MAX);
    Ok((
        change_id,
        seq,
        OrganizeOutput {
            change_id: None,
            changed,
            unchanged: u32::try_from(ids.len()).unwrap_or(u32::MAX) - changed,
            results,
            problems: None,
        },
    ))
}

fn ids_where(conn: &Connection, table: &str, recipe: &str) -> R<Vec<String>> {
    let mut statement = conn
        .prepare(&format!(
            "SELECT id FROM {table} WHERE recipe_id = ?1 ORDER BY id"
        ))
        .map_err(sql)?;
    let rows = statement
        .query_map(params![recipe], |row| row.get(0))
        .map_err(sql)?;
    rows.collect::<Result<_, _>>().map_err(sql)
}

const ALL_FACETS: [&str; 5] = [STATE, TAGS, NOTES, IMAGES, COOKS];

fn merge_tx(
    conn: &Connection,
    now: i64,
    input: &MergeInput,
    actor: &Actor,
) -> R<(String, u64, MergeOutput)> {
    let keep = live_recipe_for_write(conn, &input.keep)?;
    let mut absorbed = Vec::new();
    for id in &input.absorb {
        let row = recipe_for_write(conn, id)?;
        if row.id == keep.id {
            return Err(invalid("a recipe cannot absorb itself"));
        }
        if !absorbed
            .iter()
            .any(|known: &db::RecipeRow| known.id == row.id)
        {
            absorbed.push(row);
        }
    }
    let mut ids = vec![keep.id.clone()];
    ids.extend(absorbed.iter().map(|row| row.id.clone()));
    let mut cooks = Vec::new();
    for row in &absorbed {
        cooks.extend(ids_where(conn, "recipes_cook", &row.id)?);
    }
    let before = json!({
        "recipes": meta::recipes_facets(conn, &ids, &ALL_FACETS)?,
        "own": meta::ownership(conn, &cooks)?,
    });

    let mut moved = MergeMoved::default();
    let mut keep_tags = tags_of(conn, &keep.id)?;
    let mut notes = keep.notes.clone();
    let mut cook_count = keep.cook_count;
    let mut last_cooked = keep.last_cooked_at;
    let mut position = u32::try_from(images_of(conn, &keep.id)?.len()).unwrap_or(0);
    for row in &absorbed {
        for tag in tags_of(conn, &row.id)? {
            if !keep_tags.contains(&tag) {
                keep_tags.push(tag);
                moved.tags += 1;
            }
        }
        for image in images_of(conn, &row.id)? {
            conn.execute(
                "UPDATE recipes_image SET recipe_id = ?2, position = ?3 WHERE id = ?1",
                params![image.id, keep.id, position],
            )
            .map_err(sql)?;
            position += 1;
            moved.images += 1;
        }
        let cooks_moved = conn
            .execute(
                "UPDATE recipes_cook SET recipe_id = ?2 WHERE recipe_id = ?1",
                params![row.id, keep.id],
            )
            .map_err(sql)?;
        moved.cooks += u32::try_from(cooks_moved).unwrap_or(u32::MAX);
        cook_count += row.cook_count;
        last_cooked = last_cooked.max(row.last_cooked_at);
        if input.notes.unwrap_or_default() == MergeNotes::Append && !row.notes.trim().is_empty() {
            if !notes.trim().is_empty() {
                notes.push_str("\n\n");
            }
            notes.push_str(&format!("From {}:\n{}", row.title, row.notes.trim()));
        }
        conn.execute(
            "UPDATE recipes_recipe SET state = 'trashed', state_changed_at = ?2, merged_into = ?3,
               cover_image = NULL WHERE id = ?1",
            params![row.id, now, keep.id],
        )
        .map_err(sql)?;
        conn.execute(
            "DELETE FROM recipes_recipe_tag WHERE recipe_id = ?1",
            params![row.id],
        )
        .map_err(sql)?;
    }
    // A kept recipe without a stored source inherits the first one absorbed.
    if keep.source_item.is_none()
        && let Some(source) = absorbed.iter().find_map(|row| row.source_item.clone())
    {
        conn.execute(
            "UPDATE recipes_recipe SET source_item = ?2 WHERE id = ?1",
            params![keep.id, source],
        )
        .map_err(sql)?;
    }
    keep_tags.sort();
    db::set_tags(conn, &keep.id, &keep_tags)?;
    let notes_changed = notes != keep.notes;
    conn.execute(
        "UPDATE recipes_recipe SET notes = ?2, notes_rev = notes_rev + ?3, cook_count = ?4,
           last_cooked_at = ?5 WHERE id = ?1",
        params![
            keep.id,
            notes,
            i64::from(notes_changed),
            cook_count,
            last_cooked
        ],
    )
    .map_err(sql)?;
    let seq = next_seq(conn)?;
    let mut touched = Map::new();
    for id in &ids {
        db::touch(conn, id, seq, now)?;
        refresh_fts(conn, id)?;
        touched.insert(id.clone(), json!(ALL_FACETS));
    }
    tags::touch(&mut touched, &keep_tags, &[]);
    let after = json!({
        "recipes": meta::recipes_facets(conn, &ids, &ALL_FACETS)?,
        "own": meta::ownership(conn, &cooks)?,
    });
    let trashed: Vec<String> = absorbed.iter().map(|row| row.id.clone()).collect();
    let change = log_change(
        conn,
        seq,
        now,
        &Change {
            actor,
            summary: format!("Merge {} into {}", trashed.join(", "), keep.title),
            kind: "meta",
            touched: Value::Object(touched),
            before,
            after,
        },
    )?;
    Ok((
        change,
        seq,
        MergeOutput {
            change_id: None,
            id: keep.id,
            moved,
            trashed,
        },
    ))
}

struct LoggedChange {
    id: String,
    seq: u64,
    at: i64,
    summary: String,
    kind: String,
    touched: Value,
    before: Value,
    after: Value,
    undone_by: Option<String>,
}

fn parse(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or(Value::Null)
}

fn logged(row: &rusqlite::Row<'_>) -> rusqlite::Result<LoggedChange> {
    let (touched, before, after): (String, String, String) =
        (row.get(5)?, row.get(6)?, row.get(7)?);
    Ok(LoggedChange {
        id: row.get(0)?,
        seq: db::from_sql_seq(row.get(1)?),
        at: row.get(2)?,
        summary: row.get(3)?,
        kind: row.get(4)?,
        touched: parse(&touched),
        before: parse(&before),
        after: parse(&after),
        undone_by: row.get(8)?,
    })
}

const CHANGE_COLUMNS: &str = "id, seq, at, summary, kind, touched, before, after, undone_by";

/// Whether two `touched` maps name the same facet of the same thing.
fn overlaps(a: &Value, b: &Value) -> bool {
    let (Some(a), Some(b)) = (a.as_object(), b.as_object()) else {
        return false;
    };
    a.iter().any(|(key, facets)| {
        let Some(other) = b.get(key).and_then(Value::as_array) else {
            return false;
        };
        let Some(facets) = facets.as_array() else {
            return false;
        };
        // Two changes that merely used the same tag do not collide; a change
        // to the tag's registry row collides with any other touch of it.
        if key.starts_with(meta::TAG_KEY_PREFIX) {
            let registry = json!(REGISTRY);
            return facets.contains(&registry) || other.contains(&registry);
        }
        facets.iter().any(|facet| other.contains(facet))
    })
}

struct Undone {
    undo: String,
    seq: u64,
    restored: u32,
    ids: Vec<String>,
    content: Option<(u32, String)>,
}

fn undo_tx(conn: &Connection, now: i64, change_id: &str, actor: &Actor) -> R<Undone> {
    let change = rusqlite::OptionalExtension::optional(conn.query_row(
        &format!("SELECT {CHANGE_COLUMNS} FROM recipes_change WHERE id = ?1"),
        params![change_id],
        logged,
    ))
    .map_err(sql)?
    .ok_or_else(|| not_found(format!("no change has id {change_id:?}")))?;
    if now - change.at > RETENTION_MS {
        return Err(not_found(format!(
            "{change_id} is older than 30 days and can no longer be undone"
        )));
    }
    if let Some(by) = &change.undone_by {
        return Err(reject(
            "undo_conflict",
            format!("{change_id} was already undone by {by}; undo {by} to redo it"),
        ));
    }
    let mut statement = conn
        .prepare(&format!(
            "SELECT {CHANGE_COLUMNS} FROM recipes_change
             WHERE seq > ?1 AND undone_by IS NULL ORDER BY seq"
        ))
        .map_err(sql)?;
    let newer: Vec<String> = statement
        .query_map(params![db::sql_seq(change.seq)], logged)
        .map_err(sql)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql)?
        .into_iter()
        // An undo entry only puts back what its change wrote.
        .filter(|later| later.after.get("undoes").is_none())
        .filter(|later| overlaps(&change.touched, &later.touched))
        .map(|later| later.id)
        .collect();
    if !newer.is_empty() {
        return Err(reject(
            "undo_conflict",
            format!(
                "newer changes touched what {change_id} touched: {}; undo those first, newest first",
                newer.join(", ")
            ),
        ));
    }
    let seq = next_seq(conn)?;
    let summary = format!("Undo: {}", change.summary);
    let mut wrote: Vec<String> = Vec::new();
    let mut content: Option<(u32, String)> = None;
    let (kind, touched, before, mut after, restored) = match change.kind.as_str() {
        "create" => {
            let id = change
                .after
                .get("recipe")
                .and_then(Value::as_str)
                .ok_or_else(|| not_found("the change names no recipe"))?
                .to_owned();
            let ids = [id.clone()];
            let before = json!({ "recipes": meta::recipes_facets(conn, &ids, &[STATE])? });
            crate::organize::set_state(conn, &id, "trashed", now)?;
            db::touch(conn, &id, seq, now)?;
            let after = json!({ "recipes": meta::recipes_facets(conn, &ids, &[STATE])? });
            wrote.push(id.clone());
            ("meta", json!({ id: [STATE] }), before, after, 1)
        }
        "content" => {
            let id = change
                .after
                .get("recipe")
                .and_then(Value::as_str)
                .ok_or_else(|| not_found("the change names no recipe"))?
                .to_owned();
            let rev = |value: &Value| {
                value
                    .get("rev")
                    .and_then(Value::as_u64)
                    .and_then(|rev| u32::try_from(rev).ok())
                    .unwrap_or(0)
            };
            let (from, to) = (rev(&change.before), rev(&change.after));
            let row = live_recipe_for_write(conn, &id)?;
            if row.head_rev != to {
                return Err(reject(
                    "undo_conflict",
                    format!(
                        "{id} is at revision {}, past the revision {to} this change wrote; use recipes_revert",
                        row.head_rev
                    ),
                ));
            }
            let revision = if from == to {
                db::touch(conn, &id, seq, now)?;
                to
            } else {
                let doc = doc_at(conn, &id, from)?;
                write_revision(conn, now, seq, &row, &doc, &summary, actor)?
            };
            wrote.push(id.clone());
            content = Some((
                revision,
                omni_recipe_format::render_list(&doc_at(conn, &id, revision)?, 1.0),
            ));
            (
                "content",
                json!({ id.clone(): ["content"] }),
                json!({ "recipe": id, "rev": to }),
                json!({ "recipe": id, "rev": revision }),
                1,
            )
        }
        // A cook is history, not a change to take back.
        "cook" => {
            return Err(reject(
                "not_undoable",
                format!("{change_id} records a finished cook, which cannot be undone"),
            ));
        }
        _ => {
            let before = meta::like(conn, &change.before)?;
            let written = meta::apply(conn, &change.before, seq, now)?;
            let restored = written
                .len()
                .max(usize::from(change.before.get(REGISTRY).is_some()));
            wrote = written;
            (
                "meta",
                change.touched.clone(),
                before,
                change.before.clone(),
                u32::try_from(restored).unwrap_or(u32::MAX),
            )
        }
    };
    if let Value::Object(map) = &mut after {
        map.insert("undoes".to_owned(), json!(change.id));
    }
    let undo = log_change(
        conn,
        seq,
        now,
        &Change {
            actor,
            summary,
            kind,
            touched,
            before,
            after,
        },
    )?;
    conn.execute(
        "UPDATE recipes_change SET undone_by = ?2 WHERE id = ?1",
        params![change.id, undo],
    )
    .map_err(sql)?;
    // Undoing an undo makes the original change stand again.
    if let Some(original) = change.after.get("undoes").and_then(Value::as_str) {
        conn.execute(
            "UPDATE recipes_change SET undone_by = NULL WHERE id = ?1",
            params![original],
        )
        .map_err(sql)?;
    }
    Ok(Undone {
        undo,
        seq,
        restored,
        ids: wrote,
        content,
    })
}

impl Recipes {
    pub async fn organize(&self, input: OrganizeInput, actor: Actor) -> R<OrganizeOutput> {
        let dry = input.dry_run.unwrap_or(false);
        // A dry run reports the tags the real call would refuse as problems,
        // and previews the rest as if they were registered.
        let mut input = input;
        let mut problems = Vec::new();
        if dry && !input.create_tags.unwrap_or(false) {
            let add = tags::names(input.add_tags.as_deref().unwrap_or_default())?;
            problems = self.query(move |conn| tags::problems(conn, &add)).await?;
            if !problems.is_empty() {
                input.create_tags = Some(true);
            }
        }
        let (change, seq, mut output) = self
            .transact(dry, move |conn, now| organize_tx(conn, now, &input, &actor))
            .await?;
        if !dry && change.is_some() {
            output.change_id = change;
            self.hub.library(seq);
        }
        output.problems = (!problems.is_empty()).then_some(problems);
        Ok(output)
    }

    pub async fn merge(&self, input: MergeInput, actor: Actor) -> R<MergeOutput> {
        let dry = input.dry_run.unwrap_or(false);
        let (change, seq, mut output) = self
            .transact(dry, move |conn, now| merge_tx(conn, now, &input, &actor))
            .await?;
        if !dry {
            output.change_id = Some(change);
            self.hub.library(seq);
        }
        Ok(output)
    }

    pub async fn undo(&self, input: UndoInput, actor: Actor) -> R<UndoOutput> {
        let dry = input.dry_run.unwrap_or(false);
        let change_id = input.change_id.clone();
        let done = self
            .transact(dry, move |conn, now| {
                undo_tx(conn, now, &input.change_id, &actor)
            })
            .await?;
        if !dry {
            self.hub.library(done.seq);
        }
        let (revision, text) = done.content.unzip();
        Ok(UndoOutput {
            undone: !dry,
            change_id,
            restored: done.restored,
            undo_change_id: (!dry).then_some(done.undo),
            ids: done.ids,
            // A dry run wrote no revision to report.
            revision: revision.filter(|_| !dry),
            text,
        })
    }
}
