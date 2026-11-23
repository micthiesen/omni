//! Content: creating recipes, the three ways to write a revision, reading one
//! recipe, and history.

use omni_recipe_format::{
    CellKind, Code, Level, ListOptions, Marks, Op, Problem, RecipeDoc, Report, apply_ops,
    chart_ascii, chart_blocks, diff_summary, diff_summary_with_ops, render_list, render_list_with,
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
    self, Actor, CURRENT_FORMAT, Change, ImageRow, RecipeRow, doc_at, encode_doc, image_infos,
    images_of, iso, live_recipe_for_write, log_change, next_seq, normal_url, recipe_for_read,
    refresh_fts, tags_of, time_info, touched_one,
};
use crate::error::{R, RecipesError, invalid, not_found, reject, sql};
use crate::fidelity;
use crate::household;
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
    if factor.is_finite() && (0.05..=100.0).contains(&factor) {
        Ok(factor)
    } else {
        Err(invalid(
            "the scale (or servings divided by the recipe's yield) must be between 0.05 and 100",
        ))
    }
}

/// The ASCII chart of every block, as `view: "chart"` returns it; with
/// `ids`, each step cell carries its step id and each input row the id it
/// refers to.
pub(crate) fn render_chart(doc: &RecipeDoc, factor: f64, ids: bool) -> String {
    let charts: Vec<String> = chart_blocks(doc, factor)
        .into_iter()
        .map(|mut block| {
            if ids {
                for cell in &mut block.cells {
                    let id = match cell.kind {
                        CellKind::Step | CellKind::Setup => cell.step.clone(),
                        CellKind::Leaf | CellKind::Ref => {
                            let after = cell.key.split_once('/').map_or("", |(_, rest)| rest);
                            after.split(':').next().unwrap_or(after).to_owned()
                        }
                    };
                    if !id.is_empty() {
                        cell.label = format!("{} [{id}]", cell.label);
                    }
                }
            }
            chart_ascii(&block, &Marks::new())
        })
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
        let report = household::check(&doc, None, input.keep_diet_conflicts.unwrap_or(false));
        let time = time_info(&doc);
        if !report.is_valid() && !dry {
            return Err(invalid_recipe(&report));
        }
        if input.from_inbox.is_some() && input.source_text.is_some() {
            return Err(invalid("pass fromInbox or sourceText, not both"));
        }
        let tag_names = tags::names(input.tags.as_deref().unwrap_or_default())?;
        if dry {
            // A dry run reports everything the real call would refuse, at
            // once: the document's problems, tags the registry lacks, and a
            // source or document that already exists.
            let (document, names, from_inbox) =
                (doc.clone(), tag_names.clone(), input.from_inbox.clone());
            let (create_tags, allow_duplicate) = (
                input.create_tags.unwrap_or(false),
                input.allow_duplicate.unwrap_or(false),
            );
            let mut problems = report.errors.clone();
            problems.extend(
                self.query(move |conn| {
                    let mut found = Vec::new();
                    if !create_tags {
                        found.extend(tags::problems(conn, &names)?);
                    }
                    if !allow_duplicate {
                        let item = source_item(conn, from_inbox.as_deref())?;
                        let (_, digest) = encode_doc(&document)?;
                        if let (_, Some(duplicate)) =
                            source_checks(conn, &document, item.as_ref(), &digest)?
                        {
                            found.push(Problem {
                                code: Code::DuplicateSource,
                                path: "source".to_owned(),
                                message: duplicate.to_string(),
                                hint: Some(
                                    "edit the existing recipe, or pass allowDuplicate".to_owned(),
                                ),
                                level: Level::Error,
                            });
                        }
                    }
                    Ok(found)
                })
                .await?,
            );
            if !problems.is_empty() {
                return Ok(CreateOutput {
                    saved: false,
                    id: None,
                    revision: None,
                    change_id: None,
                    problems: Some(problems),
                    time,
                    warnings: report.warnings.clone(),
                    fidelity: fidelity::unchecked("the write would be refused"),
                    text: if report.is_valid() {
                        render_list(&doc, 1.0)
                    } else {
                        String::new()
                    },
                });
            }
        }
        let text = render_list(&doc, 1.0);
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
        let (id, seq, change) = self
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
            change_id: (!dry).then_some(change),
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
            keep_diet: input.keep_diet_conflicts.unwrap_or(false),
            kind: ContentKind::Edit(input.ops),
        };
        let mut result = self
            .write_content(write, actor, input.dry_run.unwrap_or(false))
            .await?;
        // An edit is usually followed by another: its text carries the ids.
        if let Some(doc) = &result.doc {
            result.text = render_list_with(doc, 1.0, ListOptions { ids: true });
        }
        Ok(result.output(None))
    }

    pub async fn replace(&self, input: ReplaceInput, actor: Actor) -> R<WriteOutput> {
        let dry = input.dry_run.unwrap_or(false);
        let write = ContentWrite {
            id: input.id,
            expected: input.expected_revision,
            summary: input.summary,
            keep_diet: input.keep_diet_conflicts.unwrap_or(false),
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
            // An earlier revision of this recipe is the owner's own history.
            keep_diet: true,
            kind: ContentKind::Revert(input.to_revision),
        };
        let result = self
            .write_content(write, actor, input.dry_run.unwrap_or(false))
            .await?;
        Ok(RevertOutput {
            saved: result.saved,
            id: result.id,
            revision: result.revision,
            change_id: result.change_id,
            problems: result.problems,
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
            result.change_id = None;
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
            let options = ListOptions {
                ids: input.ids.unwrap_or(false),
            };
            let images = images_of(conn, &row.id)?;
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
                pinned: crate::pins::place(conn, &row.id)?,
                    warnings: household::check(&doc, Some(&doc), false).warnings,
                linked,
                scale: (view != View::Document).then_some(factor),
                text: (view == View::Text).then(|| render_list_with(&doc, factor, options)),
                chart: (view == View::Chart).then(|| render_chart(&doc, factor, options.ids)),
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
                            recipes: u32::try_from(touched_recipes(&touched).len())
                                .unwrap_or(u32::MAX),
                            recipe_ids: touched_recipes(&touched).into_iter().take(20).collect(),
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

/// The recipes a change's `touched` map names.
fn touched_recipes(touched: &str) -> Vec<String> {
    serde_json::from_str::<serde_json::Value>(touched)
        .ok()
        .and_then(|value| {
            value.as_object().map(|map| {
                map.keys()
                    .filter(|key| key.starts_with(ids::RECIPE))
                    .cloned()
                    .collect()
            })
        })
        .unwrap_or_default()
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

/// The inbox item a create names, when it is one agents can see.
fn source_item(conn: &Connection, from_inbox: Option<&str>) -> R<Option<inbox::Item>> {
    match from_inbox {
        Some(item_id) => Ok(Some(
            inbox::item(conn, item_id)?
                .filter(|item| item.state != "uploading")
                .ok_or_else(|| not_found(format!("no inbox item has id {item_id:?}")))?,
        )),
        None => Ok(None),
    }
}

/// The source URL a new recipe is filed under, and the refusal a duplicate
/// earns (which `allowDuplicate` waives): the inbox item already became a
/// recipe, a recipe has the same source URL, or the exact document exists.
fn source_checks(
    conn: &Connection,
    doc: &RecipeDoc,
    item: Option<&inbox::Item>,
    digest: &str,
) -> R<(Option<String>, Option<RecipesError>)> {
    let first = match item.and_then(|item| item.recipe_id.as_deref()) {
        Some(existing) => db::recipe(conn, existing)?
            .filter(|row| row.state != "trashed")
            .map(|row| duplicate(&row.id, &row.title, "this inbox item")),
        None => None,
    };
    // Both the URL the document names and the one the page was read from.
    let doc_url = doc.source.url.as_deref().and_then(normal_url);
    let item_url = item
        .and_then(|item| item.url())
        .and_then(|url| normal_url(&url));
    let source_url = doc_url.clone().or_else(|| item_url.clone());
    if first.is_some() {
        return Ok((source_url, first));
    }
    {
        let wanted: Vec<&String> = doc_url.iter().chain(&item_url).collect();
        if !wanted.is_empty() {
            let mut statement = conn
                .prepare(
                    "SELECT r.id, r.title, r.source_url, i.payload FROM recipes_recipe r
                     LEFT JOIN recipes_inbox i ON i.id = r.source_item AND i.kind = 'url'
                     WHERE r.state != 'trashed' ORDER BY r.created_at",
                )
                .map_err(sql)?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                })
                .map_err(sql)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql)?;
            for (id, title, stored_url, payload) in rows {
                let page = payload
                    .and_then(|payload| serde_json::from_str::<serde_json::Value>(&payload).ok())
                    .and_then(|payload| {
                        ["finalUrl", "url"].iter().find_map(|key| {
                            payload
                                .get(*key)
                                .and_then(|v| v.as_str())
                                .map(str::to_owned)
                        })
                    })
                    .and_then(|url| normal_url(&url));
                if wanted
                    .iter()
                    .any(|url| Some(*url) == stored_url.as_ref() || Some(*url) == page.as_ref())
                {
                    return Ok((
                        source_url,
                        Some(duplicate(&id, &title, "a recipe from this source URL")),
                    ));
                }
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
            return Ok((
                source_url,
                Some(duplicate(&id, &title, "this exact document")),
            ));
        }
    }

    Ok((source_url, first))
}

fn create_tx(
    conn: &Connection,
    now: i64,
    create: Create,
    actor: &Actor,
) -> R<(String, u64, String)> {
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
    let created_tags = tags::require(conn, &tag_names, create_tags, now)?;
    let derived_from = match derived_from {
        Some(id) => Some(recipe_for_read(conn, &id)?.0.id),
        None => None,
    };
    let item = source_item(conn, from_inbox.as_deref())?;
    let (stored, digest) = encode_doc(&doc)?;
    let (source_url, duplicate) = source_checks(conn, &doc, item.as_ref(), &digest)?;
    if !allow_duplicate && let Some(found) = duplicate {
        return Err(found);
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
    let mut touched = serde_json::Map::new();
    touched.insert(
        id.clone(),
        json!(["content", "state", "tags", "notes", "images"]),
    );
    tags::touch(&mut touched, &tag_names, &created_tags);
    if let Some(item) = &item {
        adopt_images(conn, now, &id, item)?;
        inbox::mark_imported(conn, now, &item.id, &id)?;
    }
    refresh_fts(conn, &id)?;
    let change = log_change(
        conn,
        seq,
        now,
        &Change {
            actor,
            summary: format!("Created {}", doc.title),
            kind: "create",
            touched: serde_json::Value::Object(touched),
            before: json!({}),
            after: json!({ "recipe": id, "rev": 1 }),
        },
    )?;
    Ok((id, seq, change))
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
    /// The owner asked to keep ingredients outside the household diet.
    keep_diet: bool,
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
    change_id: Option<String>,
    doc: Option<RecipeDoc>,
    source_item: Option<String>,
}

impl ContentResult {
    fn output(self, fidelity: Option<Fidelity>) -> WriteOutput {
        WriteOutput {
            saved: self.saved,
            id: self.id,
            revision: self.revision,
            change_id: self.change_id,
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
        // A revision that names another source URL is found by that URL.
        "UPDATE recipes_recipe SET head_rev = ?2, title = ?3, active_min = ?4, total_min = ?5,
           updated_at = ?6, seq = ?7, source_url = coalesce(?8, source_url) WHERE id = ?1",
        params![
            row.id,
            revision,
            doc.title,
            time.active_min,
            time.total_min,
            now,
            db::sql_seq(seq),
            doc.source.url.as_deref().and_then(normal_url)
        ],
    )
    .map_err(sql)?;
    refresh_fts(conn, &row.id)?;
    Ok(revision)
}

/// The app's one content edit: a new amount for one ingredient, written as
/// a revision through the same path as an agent's edit. `Ok(Err(text))` is a
/// refusal to show the cook; nothing was written.
pub(crate) fn app_amount(
    conn: &Connection,
    now: i64,
    row: &RecipeRow,
    ingredient: &str,
    qty: f64,
    unit: Option<&str>,
    base_revision: u32,
) -> R<Result<(), String>> {
    const MOVED: &str = "the recipe changed; reload and try again";
    let head = doc_at(conn, &row.id, row.head_rev)?;
    // The cook saw an older revision: the change still stands when the
    // amount they were looking at is the amount the recipe has now.
    if base_revision < row.head_rev {
        let amount = |doc: &RecipeDoc| {
            omni_recipe_format::Index::build(doc)
                .ingredient(ingredient)
                .map(|found| (found.qty, found.unit.clone()))
        };
        let seen = match doc_at(conn, &row.id, base_revision) {
            Ok(doc) => amount(&doc),
            Err(RecipesError::Rejected { .. }) => None,
            Err(other) => return Err(other),
        };
        if seen.is_none() || seen != amount(&head) {
            return Ok(Err(MOVED.to_owned()));
        }
    }
    let ops = match omni_recipe_format::amount_ops(&head, ingredient, qty, unit) {
        Ok(ops) => ops,
        Err(refused) => return Ok(Err(refused.to_string())),
    };
    if ops.is_empty() {
        return Ok(Ok(()));
    }
    let write = ContentWrite {
        id: row.id.clone(),
        expected: row.head_rev,
        summary: format!(
            "App: {}",
            omni_recipe_format::amount_summary(&head, ingredient, qty, unit)
        ),
        keep_diet: false,
        kind: ContentKind::Edit(ops),
    };
    // Everything `content_tx` refuses, it refuses before it writes.
    match content_tx(conn, now, write, &Actor::app(), false) {
        Ok(_) => Ok(Ok(())),
        Err(RecipesError::Rejected { code, message }) => Ok(Err(if code == "invalid_recipe" {
            // The first problem line, without its number.
            message
                .lines()
                .nth(1)
                .map_or(message.as_str(), |line| line.trim_start_matches("1. "))
                .to_owned()
        } else {
            message
        })),
        Err(other) => Err(other),
    }
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
    let new = match &write.kind {
        ContentKind::Edit(ops) => {
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
    let report = household::check(&new, Some(&old), write.keep_diet);
    let mut result = ContentResult {
        id: row.id.clone(),
        revision: row.head_rev,
        saved: false,
        problems: None,
        changes: Vec::new(),
        warnings: report.warnings.clone(),
        text: String::new(),
        seq: None,
        change_id: None,
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
    result.changes = match &write.kind {
        // The operations say which ids were renamed, so a rename reads as one.
        ContentKind::Edit(ops) => diff_summary_with_ops(&old, &new, ops),
        _ => diff_summary(&old, &new),
    };
    result.text = render_list(&new, 1.0);
    if new == old {
        result.doc = Some(new);
        return Ok(result);
    }
    let seq = next_seq(conn)?;
    let revision = write_revision(conn, now, seq, &row, &new, &write.summary, actor)?;
    result.change_id = Some(log_change(
        conn,
        seq,
        now,
        &Change {
            actor,
            summary: write.summary.clone(),
            kind: "content",
            touched: touched_one(&row.id, &["content"]),
            before: json!({ "recipe": row.id, "rev": row.head_rev }),
            after: json!({ "recipe": row.id, "rev": revision }),
        },
    )?);
    result.saved = true;
    result.revision = revision;
    result.seq = Some(seq);
    result.doc = Some(new);
    Ok(result)
}

// ---------------------------------------------------------------------------
// The Crouton import's own history

/// What [`Recipes::remove_import_history`] changed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ImportHistoryRemoved {
    /// Recipes with at least one of the three changes below.
    pub recipes: u32,
    /// Documents whose source no longer names Crouton (a new revision each).
    pub sources: u32,
    /// Notes fields that lost the importer's block.
    pub notes: u32,
    /// Stored Crouton texts no longer shown as the recipe's original.
    pub originals: u32,
    /// Recipes a change could not be written to; they are as they were.
    pub skipped: u32,
}

/// The first line of the block the Crouton import appended to a recipe's
/// notes: "Imported from Crouton on 2026-11-18." followed by one "Changes:"
/// paragraph.
const IMPORT_MARK: &str = "Imported from Crouton on ";
const HISTORY_REMOVED: &str = "import_history_removed";

/// `notes` without the importer's block: the owner's own words before it,
/// and anything after the blank line that ends it. `None` when there is no
/// such block.
pub fn without_import_block(notes: &str) -> Option<String> {
    let lines: Vec<&str> = notes.lines().collect();
    let start = lines
        .iter()
        .position(|line| line.trim_start().starts_with(IMPORT_MARK))?;
    // The block runs to the next blank line.
    let end = lines[start..]
        .iter()
        .position(|line| line.trim().is_empty())
        .map_or(lines.len(), |blank| start + blank);
    let own = lines[..start].join("\n");
    let after = lines[end..].join("\n");
    let kept = [own.trim_end(), after.trim()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    Some(kept)
}

/// A source that no longer says the recipe came through Crouton: the page it
/// was taken from when there is one, a person or publication when only a
/// name is left, otherwise the owner's own.
pub fn without_import_provenance(
    source: &omni_recipe_format::Source,
) -> omni_recipe_format::Source {
    use omni_recipe_format::SourceKind;
    let mut source = source.clone();
    if source
        .name
        .as_deref()
        .is_some_and(|name| name.trim().eq_ignore_ascii_case("crouton"))
    {
        source.name = None;
    }
    let has_page = source
        .url
        .as_deref()
        .is_some_and(omni_recipe_format::is_web_url);
    source.kind = if has_page {
        SourceKind::Web
    } else if source.name.is_some() || source.author.is_some() || source.detail.is_some() {
        SourceKind::Person
    } else {
        SourceKind::Original
    };
    source
}

fn remove_import_history_tx(conn: &Connection, now: i64) -> R<Option<ImportHistoryRemoved>> {
    if crate::schema::meta(conn, HISTORY_REMOVED)?.is_some() {
        return Ok(None);
    }
    let actor = Actor::system();
    let mut removed = ImportHistoryRemoved::default();
    let mut statement = conn
        .prepare("SELECT id FROM recipes_recipe WHERE state != 'trashed' ORDER BY id")
        .map_err(sql)?;
    let ids: Vec<String> = statement
        .query_map([], |row| row.get(0))
        .map_err(sql)?
        .collect::<Result<_, _>>()
        .map_err(sql)?;
    for id in ids {
        let Some(row) = db::recipe(conn, &id)? else {
            continue;
        };
        let doc = doc_at(conn, &id, row.head_rev)?;
        let imported = doc.source.kind == omni_recipe_format::SourceKind::Crouton;
        let notes = without_import_block(&row.notes);
        // The stored original of an imported recipe is the Crouton text.
        let original = match &row.source_item {
            Some(item) if imported => {
                // Another recipe may show the same stored text (a merge
                // hands a source on); then it stays.
                let shared: bool = conn
                    .query_row(
                        "SELECT EXISTS (SELECT 1 FROM recipes_recipe WHERE source_item = ?1 AND id != ?2)",
                        params![item, id],
                        |row| row.get(0),
                    )
                    .map_err(sql)?;
                inbox::item(conn, item)?.filter(|item| item.kind == "text" && !shared)
            }
            _ => None,
        };
        if !imported && notes.is_none() {
            continue;
        }
        let mut changed = false;
        if imported {
            let mut cleaned = doc.clone();
            cleaned.source = without_import_provenance(&doc.source);
            let write = ContentWrite {
                id: id.clone(),
                expected: row.head_rev,
                summary: "Remove Crouton import provenance".to_owned(),
                // Nothing but the source changes; what the recipe holds stays.
                keep_diet: true,
                kind: ContentKind::Replace(Box::new(cleaned)),
            };
            match content_tx(conn, now, write, &actor, false) {
                Ok(result) if result.saved => {
                    removed.sources += 1;
                    changed = true;
                }
                Ok(_) => {}
                // A document the validator no longer takes keeps its source;
                // its notes and stored original are still cleaned below.
                Err(RecipesError::Rejected { .. }) => removed.skipped += 1,
                Err(other) => return Err(other),
            }
        }
        if notes.is_some() || original.is_some() {
            let ids = [id.clone()];
            let items: Vec<String> = original.iter().map(|item| item.id.clone()).collect();
            // The link to the stored source is part of the state facet; a
            // change to the notes alone must not stand in the way of undoing
            // a later archive.
            let mut facets: Vec<&str> = Vec::new();
            if original.is_some() {
                facets.push(crate::meta::STATE);
            }
            if notes.is_some() {
                facets.push(crate::meta::NOTES);
            }
            let wrote = facets.clone();
            let snapshot = |conn: &Connection| -> R<serde_json::Value> {
                Ok(json!({
                    "recipes": crate::meta::recipes_facets(conn, &ids, &facets)?,
                    crate::meta::INBOX: crate::meta::inbox_states(conn, &items)?,
                }))
            };
            let before = snapshot(conn)?;
            let seq = next_seq(conn)?;
            if let Some(notes) = &notes {
                conn.execute(
                    "UPDATE recipes_recipe SET notes = ?2, notes_rev = notes_rev + 1 WHERE id = ?1",
                    params![id, notes],
                )
                .map_err(sql)?;
                removed.notes += 1;
            }
            if let Some(item) = &original {
                conn.execute(
                    "UPDATE recipes_recipe SET source_item = NULL WHERE id = ?1",
                    params![id],
                )
                .map_err(sql)?;
                // Set aside like any discarded item: the purge removes it
                // after the 30 days in which this can be undone.
                conn.execute(
                    "UPDATE recipes_inbox SET state = 'discarded', resolved_at = ?2 WHERE id = ?1",
                    params![item.id, now],
                )
                .map_err(sql)?;
                removed.originals += 1;
            }
            db::touch(conn, &id, seq, now)?;
            refresh_fts(conn, &id)?;
            log_change(
                conn,
                seq,
                now,
                &Change {
                    actor: &actor,
                    summary: format!(
                        "Crouton import notes and stored original removed from {}",
                        row.title
                    ),
                    kind: "meta",
                    touched: json!({ id.clone(): wrote }),
                    before,
                    after: snapshot(conn)?,
                },
            )?;
            changed = true;
        }
        removed.recipes += u32::from(changed);
    }
    crate::schema::set_meta(conn, HISTORY_REMOVED, &now.to_string())?;
    Ok(Some(removed))
}

impl Recipes {
    /// Removes what the Crouton import left about itself, once per library:
    /// "crouton" as the kind of a recipe's source (and as its name), the
    /// block the importer appended to the notes, and the Crouton text kept
    /// as the stored original. The owner's own notes and the recipe stay.
    /// Each recipe gets an ordinary revision for its source and one logged
    /// change for the rest, so `recipes_undo` takes either back. `None`
    /// when it has run before.
    pub async fn remove_import_history(&self) -> R<Option<ImportHistoryRemoved>> {
        let before = self.query(db::current_seq).await?;
        let removed = self.transact(false, remove_import_history_tx).await?;
        let seq = self.query(db::current_seq).await?;
        if seq != before {
            self.hub.library(seq);
        }
        Ok(removed)
    }
}
