//! Content: creating recipes, the three ways to write a revision, reading one
//! recipe, and history.

use omni_recipe_format::{
    Marks, Op, RecipeDoc, Report, apply_ops, chart_ascii, chart_blocks, diff_summary, render_list,
    validate,
};
use omni_store::{DocOps as _, StoreError};
use rusqlite::{Connection, params};
use serde_json::json;

use crate::api::{
    ChangeInfo, CookSummary, CreateInput, CreateOutput, EditInput, Fidelity, GetInput, GetOutput,
    HistoryInput, HistoryOutput, LinkedRecipe, RecipeState, ReplaceInput, RevertInput,
    RevertOutput, RevisionInfo, SourceInfo, View, WriteOutput,
};
use crate::db::{
    self, Actor, CURRENT_FORMAT, Change, ImageRow, RecipeRow, adjustment_infos, doc_at, encode_doc,
    image_infos, images_of, iso, live_recipe_for_write, log_change, next_seq, normal_url,
    open_adjustments, recipe_for_read, refresh_fts, tags_of, time_info, touched_one,
};
use crate::error::{R, RecipesError, invalid, not_found, reject, sql};
use crate::fidelity;
use crate::ids;
use crate::inbox::{self, Snapshot};
use crate::service::Recipes;
use crate::tags;

/// The refusal of a document that breaks the format's rules: every problem
/// at once.
pub(crate) fn invalid_recipe(report: &Report) -> RecipesError {
    let text = report.refusal_text();
    reject(
        "invalid_recipe",
        text.strip_prefix("[invalid_recipe] ")
            .unwrap_or(&text)
            .to_owned(),
    )
}

/// The scale factor of a read or a cook: `scale`, or `servings` over the
/// document's yield.
pub(crate) fn factor(doc: &RecipeDoc, scale: Option<f64>, servings: Option<f64>) -> R<f64> {
    let factor = match (scale, servings) {
        (Some(_), Some(_)) => return Err(invalid("pass scale or servings, not both")),
        (Some(scale), None) => scale,
        (None, Some(servings)) => {
            if doc.r#yield.qty <= 0.0 {
                return Err(invalid("the recipe has no yield to scale servings from"));
            }
            servings / doc.r#yield.qty
        }
        (None, None) => 1.0,
    };
    if factor.is_finite() && (0.01..=1000.0).contains(&factor) {
        Ok(factor)
    } else {
        Err(invalid("the scale must be between 0.01 and 1000"))
    }
}

/// The ASCII chart of every block, as `view: "chart"` returns it.
pub(crate) fn render_chart(doc: &RecipeDoc, factor: f64) -> String {
    let charts: Vec<String> = chart_blocks(doc, factor)
        .iter()
        .map(|block| chart_ascii(block, &Marks::new()))
        .collect();
    format!("{}\n", charts.join("\n\n"))
}

pub(crate) fn state_of(row: &RecipeRow) -> RecipeState {
    match row.state.as_str() {
        "archived" => RecipeState::Archived,
        "trashed" => RecipeState::Trashed,
        _ => RecipeState::Active,
    }
}

enum Outcome<T> {
    Rolled(T),
    Failed(RecipesError),
}

impl<T> From<StoreError> for Outcome<T> {
    fn from(error: StoreError) -> Self {
        Outcome::Failed(error.into())
    }
}

impl Recipes {
    /// Runs `work` in one write transaction. With `dry` the transaction is
    /// rolled back after `work` succeeds and its result is still returned: a
    /// dry run executes exactly the real write and keeps nothing.
    pub(crate) async fn transact<T, F>(&self, dry: bool, work: F) -> R<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection, i64) -> R<T> + Send + 'static,
    {
        self.ready().await?;
        let result = self
            .store
            .write(move |tx| -> Result<T, Outcome<T>> {
                let value = work(tx.connection(), tx.now_ms()).map_err(Outcome::Failed)?;
                if dry {
                    Err(Outcome::Rolled(value))
                } else {
                    Ok(value)
                }
            })
            .await;
        match result {
            Ok(value) | Err(Outcome::Rolled(value)) => Ok(value),
            Err(Outcome::Failed(error)) => Err(error),
        }
    }

    /// Runs `work` against the latest committed state.
    pub(crate) async fn query<T, F>(&self, work: F) -> R<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> R<T> + Send + 'static,
    {
        self.ready().await?;
        self.store
            .read(move |docs| Ok(work(docs.connection())))
            .await?
    }

    pub async fn create(&self, input: CreateInput, actor: Actor) -> R<CreateOutput> {
        self.ready().await?;
        let dry = input.dry_run.unwrap_or(false);
        let doc = input.document;
        let report = validate(&doc);
        let time = time_info(&doc);
        if !report.is_valid() {
            if !dry {
                return Err(invalid_recipe(&report));
            }
            return Ok(CreateOutput {
                saved: false,
                id: None,
                revision: None,
                problems: Some(report.errors),
                time,
                warnings: report.warnings,
                fidelity: fidelity::unchecked("the document is not valid"),
                text: String::new(),
            });
        }
        if input.from_inbox.is_some() && input.source_text.is_some() {
            return Err(invalid("pass fromInbox or sourceText, not both"));
        }
        let text = render_list(&doc, 1.0);
        let tag_names = tags::names(input.tags.as_deref().unwrap_or_default())?;
        let snapshot = match (&input.from_inbox, &input.source_text) {
            (Some(item), _) => {
                let item = item.clone();
                Some(
                    self.query(move |conn| inbox::snapshot(conn, &item, false))
                        .await?,
                )
            }
            (None, Some(text)) if !text.trim().is_empty() => Some(Snapshot::pasted(text.clone())),
            _ => None,
        };
        let mut fidelity = fidelity::check(snapshot.as_ref(), &doc);

        let create = Create {
            doc: doc.clone(),
            tags: tag_names,
            create_tags: input.create_tags.unwrap_or(false),
            notes: input.notes.unwrap_or_default(),
            from_inbox: input.from_inbox,
            source_text: input.source_text.filter(|text| !text.trim().is_empty()),
            derived_from: input.derived_from,
            allow_duplicate: input.allow_duplicate.unwrap_or(false),
        };
        let (id, seq) = self
            .transact(dry, move |conn, now| create_tx(conn, now, create, &actor))
            .await?;
        if !dry {
            self.hub.library(seq);
        }
        let review = input.review.unwrap_or(!dry) && (!dry || input.review == Some(true));
        fidelity.review = self.review(snapshot.as_ref(), &text, review).await;
        Ok(CreateOutput {
            saved: !dry,
            id: (!dry).then_some(id),
            revision: (!dry).then_some(1),
            problems: None,
            time,
            warnings: report.warnings,
            fidelity,
            text,
        })
    }

    pub async fn edit(&self, input: EditInput, actor: Actor) -> R<WriteOutput> {
        let write = ContentWrite {
            id: input.id,
            expected: input.expected_revision,
            summary: input.summary,
            resolves: input.resolves.unwrap_or_default(),
            kind: ContentKind::Edit(input.ops),
        };
        let result = self
            .write_content(write, actor, input.dry_run.unwrap_or(false))
            .await?;
        Ok(result.output(None))
    }

    pub async fn replace(&self, input: ReplaceInput, actor: Actor) -> R<WriteOutput> {
        let dry = input.dry_run.unwrap_or(false);
        let write = ContentWrite {
            id: input.id,
            expected: input.expected_revision,
            summary: input.summary,
            resolves: input.resolves.unwrap_or_default(),
            kind: ContentKind::Replace(Box::new(input.document)),
        };
        let result = self.write_content(write, actor, dry).await?;
        let fidelity = match (&result.doc, result.problems.is_none()) {
            (Some(doc), true) => {
                let snapshot = match result.source_item.clone() {
                    Some(item) => self
                        .query(move |conn| inbox::snapshot(conn, &item, true))
                        .await
                        .ok(),
                    None => None,
                };
                let mut fidelity = fidelity::check(snapshot.as_ref(), doc);
                let review = input.review.unwrap_or(!dry) && (!dry || input.review == Some(true));
                fidelity.review = self.review(snapshot.as_ref(), &result.text, review).await;
                fidelity
            }
            _ => fidelity::unchecked("the document is not valid"),
        };
        Ok(result.output(Some(fidelity)))
    }

    pub async fn revert(&self, input: RevertInput, actor: Actor) -> R<RevertOutput> {
        let write = ContentWrite {
            id: input.id,
            expected: input.expected_revision,
            summary: format!("Revert to revision {}", input.to_revision),
            resolves: Vec::new(),
            kind: ContentKind::Revert(input.to_revision),
        };
        let result = self
            .write_content(write, actor, input.dry_run.unwrap_or(false))
            .await?;
        Ok(RevertOutput {
            saved: result.saved,
            id: result.id,
            revision: result.revision,
            changes: result.changes,
            text: result.text,
        })
    }

    async fn write_content(
        &self,
        write: ContentWrite,
        actor: Actor,
        dry: bool,
    ) -> R<ContentResult> {
        let expected = write.expected;
        let mut result = self
            .transact(dry, move |conn, now| {
                content_tx(conn, now, write, &actor, dry)
            })
            .await?;
        if dry {
            result.saved = false;
            result.revision = expected;
        } else if let Some(seq) = result.seq {
            self.hub.library(seq);
        }
        Ok(result)
    }

    pub async fn get(&self, input: GetInput) -> R<GetOutput> {
        let tz = self.tz.clone();
        self.query(move |conn| {
            let (row, redirected_from) = recipe_for_read(conn, &input.id)?;
            let revision = input.revision.unwrap_or(row.head_rev);
            let doc = doc_at(conn, &row.id, revision)?;
            let view = input.view.unwrap_or_default();
            let scaled = input.scale.is_some() || input.servings.is_some();
            if view == View::Document && scaled {
                return Err(invalid(
                    "scale and servings apply to view \"text\" or \"chart\"; the stored document is never scaled (use the rescale operation to change the base yield)",
                ));
            }
            let factor = factor(&doc, input.scale, input.servings)?;
            let images = images_of(conn, &row.id)?;
            let adjustments = open_adjustments(conn, &row.id)?;
            let source = match &row.source_item {
                Some(item) => inbox::item(conn, item)?.map(|item| SourceInfo {
                    url: item.url(),
                    item_id: item.id,
                    kind: item.kind,
                }),
                None => None,
            };
            let mut linked = Vec::new();
            for ingredient in doc.components.iter().flat_map(|c| &c.ingredients) {
                if let Some(target) = &ingredient.recipe {
                    let found = recipe_for_read(conn, target).ok().map(|(row, _)| row);
                    linked.push(LinkedRecipe {
                        ingredient: ingredient.id.clone(),
                        id: found.as_ref().map_or(target.clone(), |row| row.id.clone()),
                        title: found.as_ref().map(|row| row.title.clone()),
                        total_min: found.as_ref().map(|row| row.total_min),
                    });
                }
            }
            Ok(GetOutput {
                redirected_from,
                revision,
                head_revision: row.head_rev,
                title: doc.title.clone(),
                state: state_of(&row),
                tags: tags_of(conn, &row.id)?,
                notes: row.notes.clone(),
                source,
                derived_from: row.derived_from.clone(),
                images: image_infos(&row, &images),
                time: time_info(&doc),
                cooks: CookSummary {
                    count: row.cook_count,
                    last_on: row.last_cooked_at.map(|at| db::local_date(at, &tz)),
                },
                open_adjustments: adjustment_infos(&adjustments, &tz),
                warnings: validate(&doc).warnings,
                linked,
                scale: (view != View::Document).then_some(factor),
                text: (view == View::Text).then(|| render_list(&doc, factor)),
                chart: (view == View::Chart).then(|| render_chart(&doc, factor)),
                document: (view == View::Document).then_some(doc),
                id: row.id,
            })
        })
        .await
    }

    pub async fn history(&self, input: HistoryInput) -> R<HistoryOutput> {
        let limit = input.limit.unwrap_or(25).clamp(1, 100);
        let cursor = input.cursor.unwrap_or(0);
        self.query(move |conn| {
            let next = |total: u32| (cursor + limit < total).then_some(cursor + limit);
            let Some(id) = &input.id else {
                if input.diff.is_some() {
                    return Err(invalid("diff needs an id"));
                }
                let total: u32 = conn
                    .query_row("SELECT count(*) FROM recipes_change", [], |row| row.get(0))
                    .map_err(sql)?;
                let mut statement = conn
                    .prepare(
                        "SELECT id, at, tool, summary, touched, undone_by FROM recipes_change
                         ORDER BY seq DESC, at DESC LIMIT ?1 OFFSET ?2",
                    )
                    .map_err(sql)?;
                let changes = statement
                    .query_map(params![limit, cursor], |row| {
                        let touched: String = row.get(4)?;
                        let undone: Option<String> = row.get(5)?;
                        Ok(ChangeInfo {
                            change_id: row.get(0)?,
                            at: iso(row.get(1)?),
                            tool: row.get(2)?,
                            summary: row.get(3)?,
                            recipes: touched_recipes(&touched),
                            undone: undone.is_some(),
                        })
                    })
                    .map_err(sql)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(sql)?;
                return Ok(HistoryOutput {
                    total,
                    next_cursor: next(total),
                    changes: Some(changes),
                    ..HistoryOutput::default()
                });
            };
            let (row, _) = recipe_for_read(conn, id)?;
            let mut statement = conn
                .prepare(
                    "SELECT rev, created_at, tool, summary FROM recipes_revision
                     WHERE recipe_id = ?1 ORDER BY rev DESC LIMIT ?2 OFFSET ?3",
                )
                .map_err(sql)?;
            let revisions = statement
                .query_map(params![row.id, limit, cursor], |row| {
                    Ok(RevisionInfo {
                        revision: row.get(0)?,
                        at: iso(row.get(1)?),
                        tool: row.get(2)?,
                        summary: row.get(3)?,
                    })
                })
                .map_err(sql)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql)?;
            let diff = match input.diff {
                Some(range) => Some(diff_summary(
                    &doc_at(conn, &row.id, range.from)?,
                    &doc_at(conn, &row.id, range.to)?,
                )),
                None => None,
            };
            Ok(HistoryOutput {
                total: row.head_rev,
                next_cursor: next(row.head_rev),
                revisions: Some(revisions),
                diff,
                changes: None,
            })
        })
        .await
    }
}

/// How many recipes a change's `touched` map names.
fn touched_recipes(touched: &str) -> u32 {
    serde_json::from_str::<serde_json::Value>(touched)
        .ok()
        .and_then(|value| {
            value.as_object().map(|map| {
                map.keys()
                    .filter(|key| key.starts_with(ids::RECIPE))
                    .count()
            })
        })
        .map_or(0, |count| u32::try_from(count).unwrap_or(u32::MAX))
}

struct Create {
    doc: RecipeDoc,
    tags: Vec<String>,
    create_tags: bool,
    notes: String,
    from_inbox: Option<String>,
    source_text: Option<String>,
    derived_from: Option<String>,
    allow_duplicate: bool,
}

fn duplicate(id: &str, title: &str, why: &str) -> RecipesError {
    reject(
        "duplicate_source",
        format!(
            "{why} already exists as {id} ({title:?}); edit that recipe, or pass allowDuplicate to create another"
        ),
    )
}

fn create_tx(conn: &Connection, now: i64, create: Create, actor: &Actor) -> R<(String, u64)> {
    let Create {
        doc,
        tags: tag_names,
        create_tags,
        notes,
        from_inbox,
        source_text,
        derived_from,
        allow_duplicate,
    } = create;
    tags::require(conn, &tag_names, create_tags, now)?;
    let derived_from = match derived_from {
        Some(id) => Some(recipe_for_read(conn, &id)?.0.id),
        None => None,
    };
    let item = match &from_inbox {
        Some(item_id) => {
            let item = inbox::item(conn, item_id)?
                .filter(|item| item.state != "uploading")
                .ok_or_else(|| not_found(format!("no inbox item has id {item_id:?}")))?;
            if !allow_duplicate
                && let Some(existing) = &item.recipe_id
                && let Some(row) = db::recipe(conn, existing)?
                && row.state != "trashed"
            {
                return Err(duplicate(&row.id, &row.title, "this inbox item"));
            }
            Some(item)
        }
        None => None,
    };
    let source_url = doc.source.url.as_deref().and_then(normal_url).or_else(|| {
        item.as_ref()
            .and_then(|item| item.url())
            .and_then(|url| normal_url(&url))
    });
    let (stored, digest) = encode_doc(&doc)?;
    if !allow_duplicate {
        if let Some(url) = &source_url {
            let found: Option<(String, String)> =
                rusqlite::OptionalExtension::optional(conn.query_row(
                    "SELECT id, title FROM recipes_recipe
                     WHERE source_url = ?1 AND state != 'trashed' LIMIT 1",
                    params![url],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                ))
                .map_err(sql)?;
            if let Some((id, title)) = found {
                return Err(duplicate(&id, &title, "a recipe from this source URL"));
            }
        }
        let found: Option<(String, String)> =
            rusqlite::OptionalExtension::optional(conn.query_row(
                "SELECT r.id, r.title FROM recipes_revision v
                 JOIN recipes_recipe r ON r.id = v.recipe_id
                 WHERE v.doc_sha256 = ?1 AND r.state != 'trashed' LIMIT 1",
                params![digest],
                |row| Ok((row.get(0)?, row.get(1)?)),
            ))
            .map_err(sql)?;
        if let Some((id, title)) = found {
            return Err(duplicate(&id, &title, "this exact document"));
        }
    }

    let id = ids::new_id(ids::RECIPE);
    let seq = next_seq(conn)?;
    let source_item = match (&item, source_text) {
        (Some(item), _) => Some(item.id.clone()),
        (None, Some(text)) => Some(inbox::insert_text(conn, now, &doc.title, &text, &id)?),
        (None, None) => None,
    };
    let time = time_info(&doc);
    conn.execute(
        "INSERT INTO recipes_recipe (id, head_rev, title, state, state_changed_at, notes, notes_rev,
           source_item, source_url, derived_from, active_min, total_min, created_at, updated_at, seq)
         VALUES (?1, 1, ?2, 'active', ?3, ?4, 0, ?5, ?6, ?7, ?8, ?9, ?3, ?3, ?10)",
        params![
            id,
            doc.title,
            now,
            notes,
            source_item,
            source_url,
            derived_from,
            time.active_min,
            time.total_min,
            db::sql_seq(seq)
        ],
    )
    .map_err(sql)?;
    conn.execute(
        "INSERT INTO recipes_revision (recipe_id, rev, format, doc, doc_sha256, summary, tool, call_id, created_at)
         VALUES (?1, 1, ?2, ?3, ?4, 'Created', ?5, ?6, ?7)",
        params![id, CURRENT_FORMAT, stored, digest, actor.tool, actor.call_id, now],
    )
    .map_err(sql)?;
    db::set_tags(conn, &id, &tag_names)?;
    if let Some(item) = &item {
        adopt_images(conn, now, &id, item)?;
        inbox::mark_imported(conn, now, &item.id, &id)?;
    }
    refresh_fts(conn, &id)?;
    log_change(
        conn,
        seq,
        now,
        &Change {
            actor,
            summary: format!("Created {}", doc.title),
            kind: "create",
            touched: touched_one(&id, &["content", "state", "tags", "notes", "images"]),
            before: json!({}),
            after: json!({ "recipe": id, "rev": 1 }),
        },
    )?;
    Ok((id, seq))
}

/// Gives a new recipe its inbox item's images: a page's lead image becomes
/// the cover, captured photos become source images.
fn adopt_images(conn: &Connection, now: i64, id: &str, item: &inbox::Item) -> R<()> {
    let mut position = 0;
    if let Some(lead) = item.lead_image() {
        let image = ImageRow {
            id: ids::new_id(ids::IMAGE),
            recipe_id: id.to_owned(),
            sha256: lead.sha256,
            mime: lead.mime,
            width: lead.width,
            height: lead.height,
            bytes: lead.bytes,
            role: "dish".to_owned(),
            caption: None,
            origin: format!("inbox:{}", item.id),
            color: lead.color,
            position,
            created_at: now,
        };
        db::insert_image(conn, &image)?;
        conn.execute(
            "UPDATE recipes_recipe SET cover_image = ?2 WHERE id = ?1",
            params![id, image.id],
        )
        .map_err(sql)?;
        position += 1;
    }
    for file in inbox::files(conn, &item.id)? {
        db::insert_image(
            conn,
            &ImageRow {
                id: ids::new_id(ids::IMAGE),
                recipe_id: id.to_owned(),
                sha256: file.sha256,
                mime: file.mime,
                width: file.width,
                height: file.height,
                bytes: file.bytes,
                role: "source".to_owned(),
                caption: None,
                origin: format!("inbox:{}:{}", item.id, file.idx),
                color: file.color,
                position,
                created_at: now,
            },
        )?;
        position += 1;
    }
    Ok(())
}

enum ContentKind {
    Edit(Vec<Op>),
    Replace(Box<RecipeDoc>),
    Revert(u32),
}

struct ContentWrite {
    id: String,
    expected: u32,
    summary: String,
    resolves: Vec<String>,
    kind: ContentKind,
}

struct ContentResult {
    id: String,
    revision: u32,
    saved: bool,
    problems: Option<Vec<omni_recipe_format::Problem>>,
    changes: Vec<String>,
    warnings: Vec<omni_recipe_format::Problem>,
    text: String,
    seq: Option<u64>,
    doc: Option<RecipeDoc>,
    source_item: Option<String>,
}

impl ContentResult {
    fn output(self, fidelity: Option<Fidelity>) -> WriteOutput {
        WriteOutput {
            saved: self.saved,
            id: self.id,
            revision: self.revision,
            problems: self.problems,
            changes: self.changes,
            warnings: self.warnings,
            fidelity,
            text: self.text,
        }
    }
}

/// The refusal of a stale `expectedRevision`: where the recipe is now and
/// what changed since.
fn revision_conflict(conn: &Connection, row: &RecipeRow, expected: u32) -> R<RecipesError> {
    let mut statement = conn
        .prepare(
            "SELECT rev, created_at, tool, summary FROM recipes_revision
             WHERE recipe_id = ?1 AND rev > ?2 ORDER BY rev",
        )
        .map_err(sql)?;
    let since = statement
        .query_map(params![row.id, expected], |row| {
            let rev: u32 = row.get(0)?;
            let at: i64 = row.get(1)?;
            let tool: String = row.get(2)?;
            let summary: String = row.get(3)?;
            Ok(format!("revision {rev} ({}, {tool}): {summary}", iso(at)))
        })
        .map_err(sql)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql)?;
    let since = if since.is_empty() {
        format!("it has no revision {expected}")
    } else {
        format!("since revision {expected}: {}", since.join("; "))
    };
    Ok(reject(
        "revision_conflict",
        format!(
            "{} is at revision {}, not {expected}; {since}. Re-read it and apply your change to revision {}",
            row.id, row.head_rev, row.head_rev
        ),
    ))
}

/// Writes `doc` as the newest revision of `row` and returns the new revision
/// number. The caller logs the change.
pub(crate) fn write_revision(
    conn: &Connection,
    now: i64,
    seq: u64,
    row: &RecipeRow,
    doc: &RecipeDoc,
    summary: &str,
    actor: &Actor,
) -> R<u32> {
    let revision = row.head_rev + 1;
    let (stored, digest) = encode_doc(doc)?;
    let time = time_info(doc);
    conn.execute(
        "INSERT INTO recipes_revision (recipe_id, rev, format, doc, doc_sha256, summary, tool, call_id, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            row.id,
            revision,
            CURRENT_FORMAT,
            stored,
            digest,
            summary,
            actor.tool,
            actor.call_id,
            now
        ],
    )
    .map_err(sql)?;
    conn.execute(
        "UPDATE recipes_recipe SET head_rev = ?2, title = ?3, active_min = ?4, total_min = ?5,
           updated_at = ?6, seq = ?7 WHERE id = ?1",
        params![
            row.id,
            revision,
            doc.title,
            time.active_min,
            time.total_min,
            now,
            db::sql_seq(seq)
        ],
    )
    .map_err(sql)?;
    refresh_fts(conn, &row.id)?;
    Ok(revision)
}

fn content_tx(
    conn: &Connection,
    now: i64,
    write: ContentWrite,
    actor: &Actor,
    dry: bool,
) -> R<ContentResult> {
    let row = live_recipe_for_write(conn, &write.id)?;
    if row.head_rev != write.expected {
        return Err(revision_conflict(conn, &row, write.expected)?);
    }
    let old = doc_at(conn, &row.id, row.head_rev)?;
    let mut renames: Vec<(String, String)> = Vec::new();
    let new = match &write.kind {
        ContentKind::Edit(ops) => {
            for op in ops {
                if let Op::Rename { target, to } = op {
                    renames.push((target.clone(), to.clone()));
                }
            }
            apply_ops(&old, ops).map_err(|error| reject("op_failed", error.to_string()))?
        }
        ContentKind::Replace(doc) => (**doc).clone(),
        ContentKind::Revert(to) => {
            if *to >= row.head_rev {
                return Err(invalid(format!(
                    "toRevision must be earlier than the current revision {}",
                    row.head_rev
                )));
            }
            doc_at(conn, &row.id, *to)?
        }
    };
    let report = validate(&new);
    let mut result = ContentResult {
        id: row.id.clone(),
        revision: row.head_rev,
        saved: false,
        problems: None,
        changes: Vec::new(),
        warnings: report.warnings.clone(),
        text: String::new(),
        seq: None,
        doc: None,
        source_item: row.source_item.clone(),
    };
    if !report.is_valid() {
        if !dry {
            return Err(invalid_recipe(&report));
        }
        result.problems = Some(report.errors);
        return Ok(result);
    }
    result.changes = diff_summary(&old, &new);
    result.text = render_list(&new, 1.0);
    for adjustment in &write.resolves {
        let open: Option<String> = rusqlite::OptionalExtension::optional(conn.query_row(
            "SELECT id FROM recipes_adjustment WHERE id = ?1 AND recipe_id = ?2 AND state = 'open'",
            params![adjustment, row.id],
            |row| row.get(0),
        ))
        .map_err(sql)?;
        if open.is_none() {
            return Err(not_found(format!(
                "{} has no open adjustment {adjustment:?}",
                row.id
            )));
        }
    }
    if new == old && write.resolves.is_empty() {
        result.doc = Some(new);
        return Ok(result);
    }
    let seq = next_seq(conn)?;
    let revision = if new == old {
        db::touch(conn, &row.id, seq, now)?;
        row.head_rev
    } else {
        write_revision(conn, now, seq, &row, &new, &write.summary, actor)?
    };
    for (from, to) in &renames {
        conn.execute(
            "UPDATE recipes_adjustment SET target = ?3
             WHERE recipe_id = ?1 AND state = 'open' AND target = ?2",
            params![row.id, from, to],
        )
        .map_err(sql)?;
    }
    for adjustment in &write.resolves {
        conn.execute(
            "UPDATE recipes_adjustment SET state = 'resolved', resolved_rev = ?2 WHERE id = ?1",
            params![adjustment, revision],
        )
        .map_err(sql)?;
    }
    log_change(
        conn,
        seq,
        now,
        &Change {
            actor,
            summary: write.summary.clone(),
            kind: "content",
            touched: touched_one(&row.id, &["content"]),
            before: json!({ "recipe": row.id, "rev": row.head_rev }),
            after: json!({ "recipe": row.id, "rev": revision, "resolved": write.resolves }),
        },
    )?;
    result.saved = true;
    result.revision = revision;
    result.seq = Some(seq);
    result.doc = Some(new);
    Ok(result)
}
