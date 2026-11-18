//! The tag registry: every tag a recipe carries is a registered name in one
//! of seven groups. Agents maintain it; nothing is seeded.

use rusqlite::{Connection, params};
use serde_json::json;

use crate::api::{TagGroup, TagInfo, TagOp, TagsEditInput, TagsEditOutput};
use crate::db::{self, Actor, Change, log_change, next_seq, refresh_fts, tag_name};
use crate::error::{R, invalid, reject, sql};
use crate::meta::{self, REGISTRY, TAG_KEY_PREFIX, TAGS, USE};
use crate::service::Recipes;

/// Normalised, deduplicated tag names.
pub fn names(raw: &[String]) -> R<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for name in raw {
        let name = tag_name(name)?;
        if !out.contains(&name) {
            out.push(name);
        }
    }
    out.sort();
    Ok(out)
}

fn exists(conn: &Connection, name: &str) -> R<bool> {
    conn.query_row(
        "SELECT count(*) FROM recipes_tag WHERE name = ?1",
        params![name],
        |row| row.get::<_, i64>(0),
    )
    .map(|count| count > 0)
    .map_err(sql)
}

fn group(raw: &str) -> TagGroup {
    TagGroup::ALL
        .into_iter()
        .find(|group| group.as_str() == raw)
        .unwrap_or(TagGroup::Other)
}

/// The registry with the number of active recipes carrying each tag.
pub fn registry(conn: &Connection) -> R<Vec<TagInfo>> {
    let mut statement = conn
        .prepare(
            "SELECT t.name, t.grp, t.description,
               (SELECT count(*) FROM recipes_recipe_tag rt
                JOIN recipes_recipe r ON r.id = rt.recipe_id
                WHERE rt.tag = t.name AND r.state = 'active')
             FROM recipes_tag t ORDER BY t.grp, t.name",
        )
        .map_err(sql)?;
    let rows = statement
        .query_map([], |row| {
            let grp: String = row.get(1)?;
            Ok(TagInfo {
                name: row.get(0)?,
                group: group(&grp),
                description: row.get(2)?,
                count: row.get(3)?,
            })
        })
        .map_err(sql)?;
    let mut tags: Vec<TagInfo> = rows.collect::<Result<_, _>>().map_err(sql)?;
    let order = |group: TagGroup| TagGroup::ALL.iter().position(|g| *g == group);
    tags.sort_by(|a, b| (order(a.group), &a.name).cmp(&(order(b.group), &b.name)));
    Ok(tags)
}

fn distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut previous = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            let next = (previous + cost).min(row[j] + 1).min(row[j + 1] + 1);
            previous = row[j + 1];
            row[j + 1] = next;
        }
    }
    row[b.len()]
}

/// Up to five registered names close to `name`: a small edit distance for
/// the length (a third of the longer name at most), or one name starting or
/// containing the other.
pub(crate) fn nearest(conn: &Connection, name: &str) -> R<Vec<String>> {
    let length = |text: &str| text.chars().count();
    let mut scored: Vec<(usize, String)> = meta::registry_rows(conn)?
        .into_iter()
        .filter_map(|row| {
            let d = distance(name, &row.name);
            let longer = length(name).max(length(&row.name)).max(1);
            #[allow(clippy::cast_precision_loss)]
            let close = d as f64 / longer as f64 <= 0.34;
            let shares = length(name).min(length(&row.name)) >= 3
                && (row.name.contains(name) || name.contains(row.name.as_str()));
            (close || shares).then_some((d, row.name))
        })
        .collect();
    scored.sort();
    Ok(scored.into_iter().take(5).map(|(_, name)| name).collect())
}

/// One problem per name the registry lacks, as a dry run reports them.
pub(crate) fn problems(conn: &Connection, names: &[String]) -> R<Vec<omni_recipe_format::Problem>> {
    let mut found = Vec::new();
    for name in names {
        if !exists(conn, name)? {
            found.push(omni_recipe_format::Problem {
                code: omni_recipe_format::Code::UnknownTag,
                path: format!("tags.{name}"),
                message: format!("tag {name:?} is not in the registry"),
                hint: Some(format!(
                    "{}; use an existing tag, or pass createTags",
                    nearest_text(conn, name)?
                )),
                level: omni_recipe_format::Level::Error,
            });
        }
    }
    Ok(found)
}

/// "nearest: ..." for one unregistered name.
pub(crate) fn nearest_text(conn: &Connection, name: &str) -> R<String> {
    let near = nearest(conn, name)?;
    Ok(if near.is_empty() {
        "no registered tag is close to it".to_owned()
    } else {
        format!(
            "nearest: {}",
            near.iter()
                .map(|n| format!("{n:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}

/// The refusal for unregistered tags: every one of them at once, each with
/// its nearest registered names.
pub(crate) fn unknown_tags(
    conn: &Connection,
    names: &[String],
    create_hint: bool,
) -> R<crate::error::RecipesError> {
    let mut parts = Vec::new();
    for name in names {
        parts.push(format!(
            "tag {name:?} is not in the registry; {}",
            nearest_text(conn, name)?
        ));
    }
    let hint = if create_hint {
        " Use existing tags, or pass createTags to add these to the registry (group \"other\"; recipes_tags_edit sets the group)."
    } else {
        ""
    };
    Ok(reject(
        "unknown_tag",
        format!("{}.{hint}", parts.join(". ")),
    ))
}

/// Every name must be registered; with `create`, missing names are added to
/// the group `other`. Returns the names it added.
pub fn require(conn: &Connection, names: &[String], create: bool, now: i64) -> R<Vec<String>> {
    let mut missing = Vec::new();
    for name in names {
        if !exists(conn, name)? {
            missing.push(name.clone());
        }
    }
    if !create && !missing.is_empty() {
        return Err(unknown_tags(conn, &missing, true)?);
    }
    for name in &missing {
        conn.execute(
            "INSERT INTO recipes_tag (name, grp, created_at) VALUES (?1, 'other', ?2)",
            params![name, now],
        )
        .map_err(sql)?;
    }
    Ok(missing)
}

/// The key of one tag in a change's `touched` map.
pub fn tag_key(name: &str) -> String {
    format!("{TAG_KEY_PREFIX}{name}")
}

/// Records in `touched` which tags a change used on recipes and which
/// registry rows it wrote, so an undo of a registry change sees them.
pub fn touch(
    touched: &mut serde_json::Map<String, serde_json::Value>,
    used: &[String],
    wrote: &[String],
) {
    for name in used {
        touched.entry(tag_key(name)).or_insert_with(|| json!([USE]));
    }
    for name in wrote {
        touched.insert(tag_key(name), json!([REGISTRY]));
    }
}

/// The registry rows of `names` as a snapshot: each name to its row, or to
/// null when it is not registered.
fn registry_of(conn: &Connection, names: &[String]) -> R<serde_json::Value> {
    let rows = meta::registry_rows(conn)?;
    let mut out = serde_json::Map::new();
    for name in names {
        let row = rows.iter().find(|row| row.name == *name);
        out.insert(name.clone(), json!(row));
    }
    Ok(serde_json::Value::Object(out))
}

fn recipes_with(conn: &Connection, tag: &str) -> R<Vec<String>> {
    let mut statement = conn
        .prepare("SELECT recipe_id FROM recipes_recipe_tag WHERE tag = ?1 ORDER BY recipe_id")
        .map_err(sql)?;
    let rows = statement
        .query_map(params![tag], |row| row.get(0))
        .map_err(sql)?;
    rows.collect::<Result<_, _>>().map_err(sql)
}

fn known(conn: &Connection, raw: &str) -> R<String> {
    let name = tag_name(raw)?;
    if exists(conn, &name)? {
        Ok(name)
    } else {
        Err(unknown_tags(conn, &[name], false)?)
    }
}

fn tags_edit_tx(conn: &Connection, now: i64, ops: &[TagOp], actor: &Actor) -> R<(String, u64)> {
    // The recipes and registry rows any op can reach, snapshotted before
    // the first op runs. Only these are put back by an undo: tags created or
    // used since are left alone.
    let mut involved: Vec<String> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    for op in ops {
        let (on_recipes, all): (Vec<&String>, Vec<&String>) = match op {
            TagOp::Rename { from, to } => (vec![from], vec![from, to]),
            TagOp::Merge { from, into } => {
                let all: Vec<&String> = from.iter().chain([into]).collect();
                (all.clone(), all)
            }
            TagOp::Delete { name } => (vec![name], vec![name]),
            TagOp::Create { name, .. } | TagOp::SetGroup { name, .. } => (Vec::new(), vec![name]),
        };
        for name in all {
            if let Ok(name) = tag_name(name)
                && !names.contains(&name)
            {
                names.push(name);
            }
        }
        for name in on_recipes {
            if let Ok(name) = tag_name(name) {
                for id in recipes_with(conn, &name)? {
                    if !involved.contains(&id) {
                        involved.push(id);
                    }
                }
            }
        }
    }
    involved.sort();
    names.sort();
    let before = json!({
        REGISTRY: registry_of(conn, &names)?,
        "recipes": meta::recipes_facets(conn, &involved, &[TAGS])?,
    });
    let mut said: Vec<String> = Vec::new();
    for op in ops {
        match op {
            TagOp::Create {
                name,
                group,
                description,
            } => {
                let name = tag_name(name)?;
                if exists(conn, &name)? {
                    return Err(invalid(format!("tag {name:?} already exists")));
                }
                conn.execute(
                    "INSERT INTO recipes_tag (name, grp, description, created_at) VALUES (?1, ?2, ?3, ?4)",
                    params![name, group.as_str(), description, now],
                )
                .map_err(sql)?;
                said.push(format!("create {name}"));
            }
            TagOp::Rename { from, to } => {
                let from = known(conn, from)?;
                let to = tag_name(to)?;
                if exists(conn, &to)? {
                    return Err(invalid(format!(
                        "tag {to:?} already exists; use the merge operation to fold {from:?} into it"
                    )));
                }
                conn.execute(
                    "UPDATE recipes_tag SET name = ?2 WHERE name = ?1",
                    params![from, to],
                )
                .map_err(sql)?;
                conn.execute(
                    "UPDATE recipes_recipe_tag SET tag = ?2 WHERE tag = ?1",
                    params![from, to],
                )
                .map_err(sql)?;
                said.push(format!("rename {from} to {to}"));
            }
            TagOp::Merge { from, into } => {
                let into = known(conn, into)?;
                for name in from {
                    let name = known(conn, name)?;
                    if name == into {
                        continue;
                    }
                    conn.execute(
                        "INSERT OR IGNORE INTO recipes_recipe_tag (recipe_id, tag)
                         SELECT recipe_id, ?2 FROM recipes_recipe_tag WHERE tag = ?1",
                        params![name, into],
                    )
                    .map_err(sql)?;
                    conn.execute(
                        "DELETE FROM recipes_recipe_tag WHERE tag = ?1",
                        params![name],
                    )
                    .map_err(sql)?;
                    conn.execute("DELETE FROM recipes_tag WHERE name = ?1", params![name])
                        .map_err(sql)?;
                    said.push(format!("merge {name} into {into}"));
                }
            }
            TagOp::SetGroup { name, group } => {
                let name = known(conn, name)?;
                conn.execute(
                    "UPDATE recipes_tag SET grp = ?2 WHERE name = ?1",
                    params![name, group.as_str()],
                )
                .map_err(sql)?;
                said.push(format!("move {name} to {}", group.as_str()));
            }
            TagOp::Delete { name } => {
                let name = known(conn, name)?;
                conn.execute(
                    "DELETE FROM recipes_recipe_tag WHERE tag = ?1",
                    params![name],
                )
                .map_err(sql)?;
                conn.execute("DELETE FROM recipes_tag WHERE name = ?1", params![name])
                    .map_err(sql)?;
                said.push(format!("delete {name}"));
            }
        }
    }
    let seq = next_seq(conn)?;
    let mut touched = serde_json::Map::new();
    touch(&mut touched, &[], &names);
    for id in &involved {
        db::touch(conn, id, seq, now)?;
        refresh_fts(conn, id)?;
        touched.insert(id.clone(), json!([TAGS]));
    }
    let after = json!({
        REGISTRY: registry_of(conn, &names)?,
        "recipes": meta::recipes_facets(conn, &involved, &[TAGS])?,
    });
    let change = log_change(
        conn,
        seq,
        now,
        &Change {
            actor,
            summary: format!("Tags: {}", said.join(", ")),
            kind: "meta",
            touched: serde_json::Value::Object(touched),
            before,
            after,
        },
    )?;
    Ok((change, seq))
}

impl Recipes {
    pub async fn tags_edit(&self, input: TagsEditInput, actor: Actor) -> R<TagsEditOutput> {
        let dry = input.dry_run.unwrap_or(false);
        let (change, seq, tags) = self
            .transact(dry, move |conn, now| {
                let (change, seq) = tags_edit_tx(conn, now, &input.ops, &actor)?;
                Ok((change, seq, registry(conn)?))
            })
            .await?;
        if !dry {
            self.hub.library(seq);
        }
        Ok(TagsEditOutput {
            change_id: (!dry).then_some(change),
            tags,
        })
    }
}
