//! Row access shared by every part of the service. All functions take the
//! store connection, so a caller composes them inside one transaction.

use omni_recipe_format::{FORMAT, RecipeDoc, derived_time, search_text, upgrade};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use crate::api::{ImageInfo, TimeInfo};
use crate::error::{R, corrupt, not_found, reject, sql};
use crate::ids;
use crate::schema::{meta_u64, set_meta};

/// Who is writing: the tool (or `app`, `system`) and its call id.
#[derive(Clone, Debug)]
pub struct Actor {
    pub tool: &'static str,
    pub call_id: String,
}

impl Actor {
    pub fn new(tool: &'static str, call_id: impl Into<String>) -> Self {
        Self {
            tool,
            call_id: call_id.into(),
        }
    }

    pub fn app() -> Self {
        Self::new("app", "")
    }

    pub fn system() -> Self {
        Self::new("system", "")
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// An ISO timestamp for epoch milliseconds.
pub fn iso(ms: i64) -> String {
    jiff::Timestamp::from_millisecond(ms).map_or_else(|_| ms.to_string(), |t| t.to_string())
}

/// The owner's local date (YYYY-MM-DD) at epoch milliseconds.
pub fn local_date(ms: i64, tz: &jiff::tz::TimeZone) -> String {
    jiff::Timestamp::from_millisecond(ms)
        .map(|t| t.to_zoned(tz.clone()).date().to_string())
        .unwrap_or_default()
}

#[derive(Clone, Debug, PartialEq)]
pub struct RecipeRow {
    pub id: String,
    pub head_rev: u32,
    pub title: String,
    pub state: String,
    pub state_changed_at: i64,
    pub notes: String,
    pub notes_rev: u32,
    pub cover_image: Option<String>,
    pub source_item: Option<String>,
    pub source_url: Option<String>,
    pub derived_from: Option<String>,
    pub merged_into: Option<String>,
    pub active_min: u32,
    pub total_min: u32,
    pub cook_count: u32,
    pub last_cooked_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
    pub seq: u64,
}

pub const RECIPE_COLUMNS: &str = "id, head_rev, title, state, state_changed_at, notes, notes_rev, \
     cover_image, source_item, source_url, derived_from, merged_into, active_min, total_min, \
     cook_count, last_cooked_at, created_at, updated_at, seq";

pub fn recipe_from_row(row: &Row<'_>) -> rusqlite::Result<RecipeRow> {
    Ok(RecipeRow {
        id: row.get(0)?,
        head_rev: row.get(1)?,
        title: row.get(2)?,
        state: row.get(3)?,
        state_changed_at: row.get(4)?,
        notes: row.get(5)?,
        notes_rev: row.get(6)?,
        cover_image: row.get(7)?,
        source_item: row.get(8)?,
        source_url: row.get(9)?,
        derived_from: row.get(10)?,
        merged_into: row.get(11)?,
        active_min: row.get(12)?,
        total_min: row.get(13)?,
        cook_count: row.get(14)?,
        last_cooked_at: row.get(15)?,
        created_at: row.get(16)?,
        updated_at: row.get(17)?,
        seq: from_sql_seq(row.get(18)?),
    })
}

pub fn recipe(conn: &Connection, id: &str) -> R<Option<RecipeRow>> {
    conn.query_row(
        &format!("SELECT {RECIPE_COLUMNS} FROM recipes_recipe WHERE id = ?1"),
        params![id],
        recipe_from_row,
    )
    .optional()
    .map_err(sql)
}

/// Where a merged-away id lives now, following pointers through the trash
/// and past the purge.
fn merge_target(conn: &Connection, id: &str) -> R<Option<String>> {
    let mut current = id.to_owned();
    let mut moved = false;
    for _ in 0..16 {
        let next: Option<String> = match recipe(conn, &current)? {
            Some(row) => row.merged_into,
            None => conn
                .query_row(
                    "SELECT merged_into FROM recipes_redirect WHERE id = ?1",
                    params![current],
                    |row| row.get(0),
                )
                .optional()
                .map_err(sql)?,
        };
        match next {
            Some(next) => {
                current = next;
                moved = true;
            }
            None => break,
        }
    }
    Ok(moved.then_some(current))
}

/// The recipe for a read: an id that was merged away resolves to the recipe
/// it lives under, with the id asked for as `redirectedFrom`.
pub fn recipe_for_read(conn: &Connection, id: &str) -> R<(RecipeRow, Option<String>)> {
    if let Some(target) = merge_target(conn, id)?
        && let Some(row) = recipe(conn, &target)?
    {
        return Ok((row, Some(id.to_owned())));
    }
    match recipe(conn, id)? {
        Some(row) => Ok((row, None)),
        None => Err(not_found(format!("no recipe has id {id:?}"))),
    }
}

/// The recipe for a write: a merged-away id is refused with the id it now
/// lives under.
pub fn recipe_for_write(conn: &Connection, id: &str) -> R<RecipeRow> {
    if let Some(target) = merge_target(conn, id)? {
        return Err(reject(
            "merged",
            format!("{id} was merged into {target}; apply the change to {target}"),
        ));
    }
    recipe(conn, id)?.ok_or_else(|| not_found(format!("no recipe has id {id:?}")))
}

/// Like [`recipe_for_write`], and the recipe must not be in the trash.
pub fn live_recipe_for_write(conn: &Connection, id: &str) -> R<RecipeRow> {
    let row = recipe_for_write(conn, id)?;
    if row.state == "trashed" {
        return Err(reject(
            "trashed",
            format!(
                "{id} is in the trash; restore it first with recipes_organize (state \"active\") or recipes_undo"
            ),
        ));
    }
    Ok(row)
}

#[derive(Clone, Debug)]
pub struct RevisionRow {
    pub rev: u32,
    pub format: u32,
    pub doc: String,
    pub summary: String,
    pub tool: String,
    pub created_at: i64,
}

pub fn revision(conn: &Connection, id: &str, rev: u32) -> R<Option<RevisionRow>> {
    conn.query_row(
        "SELECT rev, format, doc, summary, tool, created_at FROM recipes_revision
         WHERE recipe_id = ?1 AND rev = ?2",
        params![id, rev],
        |row| {
            Ok(RevisionRow {
                rev: row.get(0)?,
                format: row.get(1)?,
                doc: row.get(2)?,
                summary: row.get(3)?,
                tool: row.get(4)?,
                created_at: row.get(5)?,
            })
        },
    )
    .optional()
    .map_err(sql)
}

/// A stored document, upgraded to the current format.
pub fn decode_doc(id: &str, row: &RevisionRow) -> R<RecipeDoc> {
    let value: Value = serde_json::from_str(&row.doc)
        .map_err(|e| corrupt(format!("{id} revision {}: {e}", row.rev)))?;
    upgrade(row.format, value).map_err(|e| corrupt(format!("{id} revision {}: {e}", row.rev)))
}

pub fn doc_at(conn: &Connection, id: &str, rev: u32) -> R<RecipeDoc> {
    let row =
        revision(conn, id, rev)?.ok_or_else(|| not_found(format!("{id} has no revision {rev}")))?;
    decode_doc(id, &row)
}

/// The stored text of a document and its digest.
pub fn encode_doc(doc: &RecipeDoc) -> R<(String, String)> {
    let text = serde_json::to_string(doc).map_err(corrupt)?;
    let digest = sha256_hex(text.as_bytes());
    Ok((text, digest))
}

/// Hands-on and start-to-finish minutes, and whether every step is timed.
pub fn time_info(doc: &RecipeDoc) -> TimeInfo {
    let clamp = |minutes: u64| u32::try_from(minutes).unwrap_or(u32::MAX);
    let (active_min, total_min) = match doc.time {
        Some(time) => (time.active_min, time.total_min),
        None => {
            let derived = derived_time(doc);
            (clamp(derived.active_min), clamp(derived.total_min))
        }
    };
    let complete = doc.time.is_some()
        || doc
            .components
            .iter()
            .flat_map(|component| &component.steps)
            .all(|step| step.time.is_some());
    TimeInfo {
        active_min,
        total_min,
        complete,
    }
}

/// A sequence number as SQLite stores it.
pub fn sql_seq(seq: u64) -> i64 {
    i64::try_from(seq).unwrap_or(i64::MAX)
}

/// A stored sequence number.
pub fn from_sql_seq(seq: i64) -> u64 {
    u64::try_from(seq).unwrap_or(0)
}

/// Takes the next library sequence number.
pub fn next_seq(conn: &Connection) -> R<u64> {
    let seq = meta_u64(conn, "seq")? + 1;
    set_meta(conn, "seq", &seq.to_string())?;
    Ok(seq)
}

pub fn current_seq(conn: &Connection) -> R<u64> {
    meta_u64(conn, "seq")
}

/// Marks `id` changed at `seq`.
pub fn touch(conn: &Connection, id: &str, seq: u64, now: i64) -> R<()> {
    conn.execute(
        "UPDATE recipes_recipe SET seq = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, sql_seq(seq), now],
    )
    .map_err(sql)?;
    Ok(())
}

pub fn tags_of(conn: &Connection, id: &str) -> R<Vec<String>> {
    let mut statement = conn
        .prepare_cached("SELECT tag FROM recipes_recipe_tag WHERE recipe_id = ?1 ORDER BY tag")
        .map_err(sql)?;
    let rows = statement
        .query_map(params![id], |row| row.get(0))
        .map_err(sql)?;
    rows.collect::<Result<_, _>>().map_err(sql)
}

pub fn set_tags(conn: &Connection, id: &str, tags: &[String]) -> R<()> {
    conn.execute(
        "DELETE FROM recipes_recipe_tag WHERE recipe_id = ?1",
        params![id],
    )
    .map_err(sql)?;
    for tag in tags {
        conn.execute(
            "INSERT OR IGNORE INTO recipes_recipe_tag (recipe_id, tag) VALUES (?1, ?2)",
            params![id, tag],
        )
        .map_err(sql)?;
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageRow {
    pub id: String,
    pub recipe_id: String,
    pub sha256: String,
    pub mime: String,
    pub width: u32,
    pub height: u32,
    pub bytes: u32,
    pub role: String,
    pub caption: Option<String>,
    pub origin: String,
    pub color: String,
    pub position: u32,
    pub created_at: i64,
}

const IMAGE_COLUMNS: &str = "id, recipe_id, sha256, mime, width, height, bytes, role, caption, \
     origin, color, position, created_at";

fn image_from_row(row: &Row<'_>) -> rusqlite::Result<ImageRow> {
    Ok(ImageRow {
        id: row.get(0)?,
        recipe_id: row.get(1)?,
        sha256: row.get(2)?,
        mime: row.get(3)?,
        width: row.get(4)?,
        height: row.get(5)?,
        bytes: row.get(6)?,
        role: row.get(7)?,
        caption: row.get(8)?,
        origin: row.get(9)?,
        color: row.get(10)?,
        position: row.get(11)?,
        created_at: row.get(12)?,
    })
}

pub fn images_of(conn: &Connection, id: &str) -> R<Vec<ImageRow>> {
    let mut statement = conn
        .prepare_cached(&format!(
            "SELECT {IMAGE_COLUMNS} FROM recipes_image WHERE recipe_id = ?1
             ORDER BY position, created_at, id"
        ))
        .map_err(sql)?;
    let rows = statement
        .query_map(params![id], image_from_row)
        .map_err(sql)?;
    rows.collect::<Result<_, _>>().map_err(sql)
}

pub fn image(conn: &Connection, image_id: &str) -> R<Option<ImageRow>> {
    conn.query_row(
        &format!("SELECT {IMAGE_COLUMNS} FROM recipes_image WHERE id = ?1"),
        params![image_id],
        image_from_row,
    )
    .optional()
    .map_err(sql)
}

pub fn insert_image(conn: &Connection, image: &ImageRow) -> R<()> {
    conn.execute(
        &format!(
            "INSERT INTO recipes_image ({IMAGE_COLUMNS})
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)"
        ),
        params![
            image.id,
            image.recipe_id,
            image.sha256,
            image.mime,
            image.width,
            image.height,
            image.bytes,
            image.role,
            image.caption,
            image.origin,
            image.color,
            image.position,
            image.created_at
        ],
    )
    .map_err(sql)?;
    Ok(())
}

/// The cover: the chosen image while it exists, otherwise the first.
pub fn cover_of<'a>(row: &RecipeRow, images: &'a [ImageRow]) -> Option<&'a ImageRow> {
    row.cover_image
        .as_deref()
        .and_then(|cover| images.iter().find(|image| image.id == cover))
        .or(images.first())
}

/// Whether a caption marks a freely licensed stand-in for the owner's photo.
pub fn is_placeholder(caption: Option<&str>) -> bool {
    caption.is_some_and(|caption| {
        caption
            .trim_start()
            .get(..12)
            .is_some_and(|head| head.eq_ignore_ascii_case("placeholder:"))
    })
}

/// SQL true when recipe `r`'s cover (the chosen one, else the first image)
/// is a placeholder; the same rule as [`is_placeholder`].
pub const PLACEHOLDER_COVER_SQL: &str = "EXISTS (SELECT 1 FROM recipes_image i WHERE i.id = \
     coalesce((SELECT c.id FROM recipes_image c WHERE c.id = r.cover_image AND c.recipe_id = r.id), \
     (SELECT f.id FROM recipes_image f WHERE f.recipe_id = r.id ORDER BY f.position, f.created_at, f.id LIMIT 1)) \
     AND lower(substr(ltrim(i.caption), 1, 12)) = 'placeholder:')";

/// Whether the recipe has a picture of the dish.
pub fn has_dish_image(conn: &Connection, id: &str) -> R<bool> {
    Ok(images_of(conn, id)?
        .iter()
        .any(|image| image.role == "dish"))
}

pub fn image_infos(row: &RecipeRow, images: &[ImageRow]) -> Vec<ImageInfo> {
    let cover = cover_of(row, images).map(|image| image.id.clone());
    images
        .iter()
        .map(|image| ImageInfo {
            image_id: image.id.clone(),
            width: image.width,
            height: image.height,
            role: image.role.clone(),
            caption: image.caption.clone(),
            cover: cover.as_deref() == Some(image.id.as_str()),
            placeholder: is_placeholder(image.caption.as_deref()),
        })
        .collect()
}

/// Rewrites the recipe's full-text row from its head revision, tags and notes.
pub fn refresh_fts(conn: &Connection, id: &str) -> R<()> {
    conn.execute("DELETE FROM recipes_fts WHERE recipe_id = ?1", params![id])
        .map_err(sql)?;
    let Some(row) = recipe(conn, id)? else {
        return Ok(());
    };
    let doc = doc_at(conn, id, row.head_rev)?;
    // Ingredient names as the shared tokeniser cuts them, so the ingredient
    // filter matches exactly what the app's own search matches.
    let ingredients: Vec<String> = doc
        .components
        .iter()
        .flat_map(|component| &component.ingredients)
        .map(|ingredient| omni_recipe_format::tokens(&ingredient.name).join(" "))
        .collect();
    let tags = tags_of(conn, id)?;
    let body = search_text(&doc);
    let source = [
        doc.source.name.as_deref(),
        doc.source.author.as_deref(),
        doc.source.detail.as_deref(),
        doc.source.url.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join("\n");
    let everything = [
        doc.title.as_str(),
        &ingredients.join("\n"),
        &tags.join(" "),
        &body,
        &row.notes,
        &source,
    ]
    .join("\n");
    conn.execute(
        "INSERT INTO recipes_fts (recipe_id, title, ingredients, tags, body, notes, source, folded)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            id,
            doc.title,
            ingredients.join("\n"),
            tags.join(" "),
            body,
            row.notes,
            source,
            // Every word as the shared tokeniser cuts it ("grandma's" is
            // "grandmas", "weißwurst" is "weisswurst"): a query term always
            // finds its word here, whatever FTS made of the original.
            omni_recipe_format::tokens(&everything).join(" ")
        ],
    )
    .map_err(sql)?;
    Ok(())
}

/// One change-log entry. `touched` maps each recipe id (or `*tags` for the
/// registry) to the facets the change wrote; an undo is refused when a newer
/// change wrote the same facet of the same thing.
pub struct Change<'a> {
    pub actor: &'a Actor,
    pub summary: String,
    pub kind: &'static str,
    pub touched: Value,
    pub before: Value,
    pub after: Value,
}

/// Records `change` at `seq` and returns its id.
pub fn log_change(conn: &Connection, seq: u64, now: i64, change: &Change<'_>) -> R<String> {
    let id = ids::new_id(ids::CHANGE);
    conn.execute(
        "INSERT INTO recipes_change (id, seq, at, tool, call_id, summary, kind, touched, before, after)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            id,
            sql_seq(seq),
            now,
            change.actor.tool,
            change.actor.call_id,
            change.summary,
            change.kind,
            change.touched.to_string(),
            change.before.to_string(),
            change.after.to_string()
        ],
    )
    .map_err(sql)?;
    Ok(id)
}

/// `{id: facets}` for one recipe.
pub fn touched_one(id: &str, facets: &[&str]) -> Value {
    json!({ id: facets })
}

/// A normalised tag name: trimmed, lowercase, inner whitespace as hyphens.
pub fn tag_name(raw: &str) -> R<String> {
    let name = raw
        .trim()
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("-");
    let valid = (1..=40).contains(&name.chars().count())
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '&' | '\''));
    if valid {
        Ok(name)
    } else {
        Err(crate::error::invalid(format!(
            "tag {raw:?} must be 1 to 40 characters: letters, digits, hyphens, underscores, & or apostrophes (spaces become hyphens)"
        )))
    }
}

/// A source URL in the form duplicates are compared in: no fragment, no
/// trailing slash, lowercase scheme and host, no `www.`.
pub fn normal_url(raw: &str) -> Option<String> {
    let mut url = url::Url::parse(raw.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    url.set_fragment(None);
    let host = url.host_str()?.trim_start_matches("www.").to_owned();
    let path = url.path().trim_end_matches('/');
    let query = url.query().map(|q| format!("?{q}")).unwrap_or_default();
    Some(format!("{host}{path}{query}"))
}

pub const CURRENT_FORMAT: u32 = FORMAT;
