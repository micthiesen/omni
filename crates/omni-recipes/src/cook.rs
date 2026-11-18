//! Cook sessions: one pinned revision and scale, and an append-only event
//! log that the app and agents both write. Every view is a projection of the
//! log. Cooking never edits the recipe: finishing turns the session's
//! overrides into adjustment records that a later revision folds in.

use omni_alerts::{PushOutcome, PushoverMessage};
use omni_api::fond::{CookActor, CookEvent, CookEventKind, CookSession, CookState};
use omni_config::PushoverChannel;
use omni_recipe_format::{Index, RecipeDoc, ingredient_line, ready_steps, render_text};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::json;

use crate::api::{
    ActiveCook, CookEventInput, CookGetInput, CookGetOutput, CookLogEntry, CookStartInput,
    CookStartOutput, CookUpdateInput, CookUpdateOutput, SessionAdjustment, SessionView, TimerInfo,
};
use crate::db::{
    self, Actor, Change, ImageRow, adjustment_infos, doc_at, iso, log_change, next_seq,
    open_adjustments, recipe_for_read,
};
use crate::error::{R, invalid, not_found, reject, sql};
use crate::events;
use crate::ids;
use crate::library::factor;
use crate::meta::{self, COOKS, IMAGES};
use crate::service::Recipes;

const LOG: &str = "Recipes";

/// A session with no event for this long closes itself.
pub const IDLE_CLOSE_MS: i64 = 18 * 60 * 60 * 1000;
/// A timer that ended longer ago than this is not announced.
pub const ALERT_WINDOW_MS: i64 = 10 * 60 * 1000;
/// How long a connected app has to answer an ended timer before it is
/// announced anyway.
pub const APP_GRACE_MS: i64 = 30 * 1000;
const MAX_EVENT_TEXT: usize = 2000;

#[derive(Clone, Debug, PartialEq)]
pub struct SessionRow {
    pub id: String,
    pub recipe_id: String,
    pub rev: u32,
    pub scale: f64,
    pub state: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub last_event_at: i64,
    pub note: Option<String>,
    pub photo_image: Option<String>,
}

const SESSION_COLUMNS: &str = "id, recipe_id, rev, scale, state, started_at, finished_at, \
     last_event_at, note, photo_image";

fn session_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow {
        id: row.get(0)?,
        recipe_id: row.get(1)?,
        rev: row.get(2)?,
        scale: row.get(3)?,
        state: row.get(4)?,
        started_at: row.get(5)?,
        finished_at: row.get(6)?,
        last_event_at: row.get(7)?,
        note: row.get(8)?,
        photo_image: row.get(9)?,
    })
}

pub fn session_row(conn: &Connection, id: &str) -> R<Option<SessionRow>> {
    conn.query_row(
        &format!("SELECT {SESSION_COLUMNS} FROM recipes_cook WHERE id = ?1"),
        params![id],
        session_from_row,
    )
    .optional()
    .map_err(sql)
}

fn sessions_where(conn: &Connection, clause: &str, value: &str) -> R<Vec<SessionRow>> {
    let mut statement = conn
        .prepare(&format!(
            "SELECT {SESSION_COLUMNS} FROM recipes_cook WHERE {clause} ORDER BY started_at, id"
        ))
        .map_err(sql)?;
    let rows = statement
        .query_map(params![value], session_from_row)
        .map_err(sql)?;
    rows.collect::<Result<_, _>>().map_err(sql)
}

fn actor_name(actor: CookActor) -> &'static str {
    match actor {
        CookActor::App => "app",
        CookActor::Agent => "agent",
        CookActor::System => "system",
    }
}

fn kind_name(kind: &CookEventKind) -> &'static str {
    match kind {
        CookEventKind::Check { .. } => "check",
        CookEventKind::Uncheck { .. } => "uncheck",
        CookEventKind::Adjust { .. } => "adjust",
        CookEventKind::Unadjust { .. } => "unadjust",
        CookEventKind::Note { .. } => "note",
        CookEventKind::Timer { .. } => "timer",
        CookEventKind::CancelTimer { .. } => "cancelTimer",
        CookEventKind::Rebase { .. } => "rebase",
        CookEventKind::Finish { .. } => "finish",
        CookEventKind::DismissAdjustment { .. } => "dismissAdjustment",
    }
}

pub fn session_events(conn: &Connection, id: &str) -> R<Vec<CookEvent>> {
    let mut statement = conn
        .prepare_cached(
            "SELECT seq, client_event_id, at, actor, payload FROM recipes_cook_event
             WHERE session_id = ?1 ORDER BY seq",
        )
        .map_err(sql)?;
    let rows = statement
        .query_map(params![id], |row| {
            let actor: String = row.get(3)?;
            let payload: String = row.get(4)?;
            Ok((
                row.get::<_, u32>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                actor,
                payload,
            ))
        })
        .map_err(sql)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql)?;
    let mut events = Vec::with_capacity(rows.len());
    for (seq, client_event_id, at, actor, payload) in rows {
        // An event a newer build wrote is skipped, not fatal.
        let Ok(kind) = serde_json::from_str::<CookEventKind>(&payload) else {
            continue;
        };
        events.push(CookEvent {
            seq,
            client_event_id,
            at,
            actor: match actor.as_str() {
                "agent" => CookActor::Agent,
                "system" => CookActor::System,
                _ => CookActor::App,
            },
            kind,
        });
    }
    Ok(events)
}

fn cook_state(row: &SessionRow) -> CookState {
    match row.state.as_str() {
        "finished" => CookState::Finished,
        "abandoned" => CookState::Abandoned,
        _ => CookState::Active,
    }
}

/// The session as the app receives it.
pub fn wire_session(conn: &Connection, row: &SessionRow) -> R<CookSession> {
    Ok(CookSession {
        id: row.id.clone(),
        recipe_id: row.recipe_id.clone(),
        revision: row.rev,
        scale: row.scale,
        state: cook_state(row),
        started_at: row.started_at,
        finished_at: row.finished_at,
        last_event_at: row.last_event_at,
        events: session_events(conn, &row.id)?,
        // The pinned revision travels with the session, so any device
        // renders the recipe the cook started on.
        doc: match doc_at(conn, &row.recipe_id, row.rev) {
            Ok(doc) => Some(serde_json::to_value(doc).map_err(crate::error::corrupt)?),
            Err(_) => None,
        },
    })
}

/// Every open session, with its events.
pub fn open_sessions(conn: &Connection) -> R<Vec<CookSession>> {
    sessions_where(conn, "state = ?1", "active")?
        .iter()
        .map(|row| wire_session(conn, row))
        .collect()
}

/// What the event log amounts to.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Projection {
    /// Every checked target, in the order first checked.
    pub checked: Vec<String>,
    /// `(step, endsAt)` of the timers still running or rung.
    pub timers: Vec<(String, i64)>,
    /// `(target, text)` of the overrides in force.
    pub adjustments: Vec<(String, String)>,
    pub notes: Vec<String>,
}

pub fn project(events: &[CookEvent]) -> Projection {
    let mut out = Projection::default();
    for event in events {
        match &event.kind {
            CookEventKind::Check { target } => {
                if !out.checked.contains(target) {
                    out.checked.push(target.clone());
                }
                // A step crossed off needs no timer.
                out.timers.retain(|(step, _)| step != target);
            }
            CookEventKind::Uncheck { target } => out.checked.retain(|known| known != target),
            CookEventKind::Adjust { target, text } => {
                match out
                    .adjustments
                    .iter_mut()
                    .find(|(known, _)| known == target)
                {
                    Some(found) => found.1.clone_from(text),
                    None => out.adjustments.push((target.clone(), text.clone())),
                }
            }
            CookEventKind::Unadjust { target } => {
                out.adjustments.retain(|(known, _)| known != target);
            }
            CookEventKind::Note { text } => out.notes.push(text.clone()),
            CookEventKind::Timer { step, ends_at } => {
                out.timers.retain(|(known, _)| known != step);
                out.timers.push((step.clone(), *ends_at));
            }
            CookEventKind::CancelTimer { step } => out.timers.retain(|(known, _)| known != step),
            CookEventKind::Finish { note } => out.notes.extend(note.clone()),
            CookEventKind::Rebase { .. } | CookEventKind::DismissAdjustment { .. } => {}
        }
    }
    out
}

fn step_ids(doc: &RecipeDoc) -> Vec<&str> {
    doc.components
        .iter()
        .flat_map(|component| &component.steps)
        .map(|step| step.id.as_str())
        .collect()
}

/// The checked targets that are steps, in written order.
fn done_steps(doc: &RecipeDoc, projection: &Projection) -> Vec<String> {
    step_ids(doc)
        .into_iter()
        .filter(|id| projection.checked.iter().any(|known| known == id))
        .map(str::to_owned)
        .collect()
}

fn view(conn: &Connection, row: &SessionRow, now: i64) -> R<SessionView> {
    let recipe = db::recipe(conn, &row.recipe_id)?
        .ok_or_else(|| not_found(format!("no recipe has id {:?}", row.recipe_id)))?;
    let doc = doc_at(conn, &row.recipe_id, row.rev)?;
    let projection = project(&session_events(conn, &row.id)?);
    let done = done_steps(&doc, &projection);
    let steps = step_ids(&doc);
    Ok(SessionView {
        session_id: row.id.clone(),
        recipe_id: row.recipe_id.clone(),
        title: doc.title.clone(),
        state: row.state.clone(),
        revision: row.rev,
        head_revision: recipe.head_rev,
        scale: row.scale,
        started_at: iso(row.started_at),
        finished_at: row.finished_at.map(iso),
        last_event_at: iso(row.last_event_at),
        steps: u32::try_from(steps.len()).unwrap_or(u32::MAX),
        ready: if row.state == "active" {
            ready_steps(&doc, &done)
        } else {
            Vec::new()
        },
        gathered: projection
            .checked
            .iter()
            .filter(|target| !steps.contains(&target.as_str()))
            .cloned()
            .collect(),
        done,
        running: projection
            .timers
            .iter()
            .map(|(step, ends_at)| TimerInfo {
                step: step.clone(),
                ends_at: iso(*ends_at),
                remaining_sec: (ends_at - now).div_euclid(1000),
            })
            .collect(),
        adjustments: projection
            .adjustments
            .iter()
            .map(|(target, text)| SessionAdjustment {
                target: target.clone(),
                text: text.clone(),
            })
            .collect(),
        notes: projection.notes,
    })
}

/// One event to append.
#[derive(Clone, Debug, PartialEq)]
pub struct Append {
    pub actor: CookActor,
    pub client_event_id: String,
    pub at: i64,
    pub kind: CookEventKind,
}

/// What a finished cook publishes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finished {
    pub session_id: String,
    pub recipe_id: String,
    pub title: String,
    pub open_adjustments: u32,
    pub at: i64,
}

/// What appending did.
#[derive(Clone, Debug, PartialEq)]
pub struct Appended {
    pub session: CookSession,
    /// The session's newest event number.
    pub seq: u32,
    /// Set when the library changed too (a finish, a dismissal).
    pub library_seq: Option<u64>,
    pub finished: Option<Finished>,
}

fn known_target(doc: &RecipeDoc, target: &str) -> bool {
    let ix = Index::build(doc);
    if ix.step(target).is_some() || ix.ingredient(target).is_some() {
        return true;
    }
    ix.steps().any(|(_, step)| {
        step.inputs
            .iter()
            .any(|input| input.key(&step.id) == target)
    })
}

/// Checks an event an agent sent against the pinned document. The app's
/// events are not checked: they were made offline against the same document
/// and cannot be sent again differently.
fn check_event(doc: &RecipeDoc, kind: &CookEventKind) -> R<()> {
    let ix = Index::build(doc);
    let text_ok = |text: &str| {
        if text.trim().is_empty() || text.chars().count() > MAX_EVENT_TEXT {
            Err(invalid(format!(
                "text must be 1 to {MAX_EVENT_TEXT} characters"
            )))
        } else {
            Ok(())
        }
    };
    let step_ok = |step: &str| {
        if ix.step(step).is_some() {
            Ok(())
        } else {
            Err(not_found(format!(
                "the session's recipe has no step {step:?}"
            )))
        }
    };
    match kind {
        CookEventKind::Check { target } | CookEventKind::Uncheck { target } => {
            if known_target(doc, target) {
                Ok(())
            } else {
                Err(not_found(format!(
                    "{target:?} is not a step, an ingredient or a \"step/ref\" input of the session's recipe"
                )))
            }
        }
        CookEventKind::Adjust { target, text } => {
            text_ok(text)?;
            if ix.step(target).is_some() || ix.ingredient(target).is_some() {
                Ok(())
            } else {
                Err(not_found(format!(
                    "{target:?} is not a step or an ingredient of the session's recipe"
                )))
            }
        }
        CookEventKind::Unadjust { .. } | CookEventKind::DismissAdjustment { .. } => Ok(()),
        CookEventKind::Note { text } => text_ok(text),
        CookEventKind::Timer { step, .. } | CookEventKind::CancelTimer { step } => step_ok(step),
        CookEventKind::Rebase { .. } => Ok(()),
        CookEventKind::Finish { note } => note.as_deref().map_or(Ok(()), text_ok),
    }
}

/// Closes `row`: `counts` makes it a cook (cook log, adjustment records).
fn finish_tx(
    conn: &Connection,
    now: i64,
    row: &SessionRow,
    note: Option<&str>,
    counts: bool,
    actor: &Actor,
) -> R<(u64, Option<Finished>)> {
    let state = if counts { "finished" } else { "abandoned" };
    conn.execute(
        "UPDATE recipes_cook SET state = ?2, finished_at = ?3, note = ?4 WHERE id = ?1",
        params![row.id, state, now, note],
    )
    .map_err(sql)?;
    let seq = next_seq(conn)?;
    if !counts {
        return Ok((seq, None));
    }
    let recipe = db::recipe(conn, &row.recipe_id)?
        .ok_or_else(|| not_found(format!("no recipe has id {:?}", row.recipe_id)))?;
    let doc = doc_at(conn, &row.recipe_id, row.rev)?;
    let projection = project(&session_events(conn, &row.id)?);
    let ix = Index::build(&doc);
    let ids = [recipe.id.clone()];
    let before = json!({ "recipes": meta::recipes_facets(conn, &ids, &[COOKS])? });
    for (target, text) in &projection.adjustments {
        let original = match (ix.step(target), ix.ingredient(target)) {
            (Some(step), _) => render_text(&ix, step, row.scale),
            (None, Some(ingredient)) => ingredient_line(&ix, ingredient, row.scale),
            (None, None) => String::new(),
        };
        conn.execute(
            "INSERT INTO recipes_adjustment (id, session_id, recipe_id, target, original, text, state, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'open', ?7)",
            params![
                ids::new_id(ids::ADJUSTMENT),
                row.id,
                recipe.id,
                target,
                original,
                text,
                now
            ],
        )
        .map_err(sql)?;
    }
    conn.execute(
        "UPDATE recipes_recipe SET cook_count = cook_count + 1, last_cooked_at = ?2,
           updated_at = ?2, seq = ?3 WHERE id = ?1",
        params![recipe.id, now, db::sql_seq(seq)],
    )
    .map_err(sql)?;
    let after = json!({ "recipes": meta::recipes_facets(conn, &ids, &[COOKS])? });
    log_change(
        conn,
        seq,
        now,
        &Change {
            actor,
            summary: format!("Cooked {}", recipe.title),
            kind: "cook",
            touched: json!({ recipe.id.clone(): [COOKS] }),
            before,
            after,
        },
    )?;
    let open = u32::try_from(open_adjustments(conn, &recipe.id)?.len()).unwrap_or(u32::MAX);
    Ok((
        seq,
        Some(Finished {
            session_id: row.id.clone(),
            recipe_id: recipe.id,
            title: recipe.title,
            open_adjustments: open,
            at: now,
        }),
    ))
}

/// Appends `events` to a session, each client event id once. `strict` (the
/// tool) refuses events on a closed session and events that name nothing;
/// the app's late or stale events are dropped instead, because its outbox
/// cannot resend them differently.
pub fn append_tx(
    conn: &Connection,
    now: i64,
    session_id: &str,
    events: Vec<Append>,
    strict: bool,
    actor: &Actor,
) -> R<Appended> {
    let mut row = session_row(conn, session_id)?
        .ok_or_else(|| not_found(format!("no cook session has id {session_id:?}")))?;
    let mut library_seq = None;
    let mut finished = None;
    let mut next: u32 = conn
        .query_row(
            "SELECT coalesce(max(seq), 0) FROM recipes_cook_event WHERE session_id = ?1",
            params![row.id],
            |row| row.get(0),
        )
        .map_err(sql)?;
    for event in events {
        let seen: i64 = conn
            .query_row(
                "SELECT count(*) FROM recipes_cook_event WHERE client_event_id = ?1",
                params![event.client_event_id],
                |row| row.get(0),
            )
            .map_err(sql)?;
        if seen > 0 {
            continue;
        }
        let dismissal = matches!(event.kind, CookEventKind::DismissAdjustment { .. });
        if row.state != "active" && !dismissal {
            if strict {
                return Err(reject(
                    "session_closed",
                    format!(
                        "{} {} at {}; start a new session with recipes_cook_start",
                        row.id,
                        if row.state == "finished" {
                            "finished"
                        } else {
                            "was closed"
                        },
                        iso(row.finished_at.unwrap_or(row.last_event_at))
                    ),
                ));
            }
            continue;
        }
        if strict {
            check_event(&doc_at(conn, &row.recipe_id, row.rev)?, &event.kind)?;
        }
        match &event.kind {
            CookEventKind::Rebase { revision } => {
                let exists = db::revision(conn, &row.recipe_id, *revision)?.is_some();
                if !exists || *revision < row.rev {
                    if strict {
                        return Err(invalid(format!(
                            "revision {revision} is not a newer revision of {}",
                            row.recipe_id
                        )));
                    }
                    continue;
                }
                conn.execute(
                    "UPDATE recipes_cook SET rev = ?2 WHERE id = ?1",
                    params![row.id, revision],
                )
                .map_err(sql)?;
                row.rev = *revision;
            }
            CookEventKind::DismissAdjustment { id } => {
                let changed = conn
                    .execute(
                        "UPDATE recipes_adjustment SET state = 'dismissed'
                         WHERE id = ?1 AND recipe_id = ?2 AND state = 'open'",
                        params![id, row.recipe_id],
                    )
                    .map_err(sql)?;
                if changed == 0 {
                    if strict {
                        return Err(not_found(format!(
                            "{} has no open adjustment {id:?}",
                            row.recipe_id
                        )));
                    }
                    continue;
                }
                let seq = next_seq(conn)?;
                db::touch(conn, &row.recipe_id, seq, now)?;
                library_seq = Some(seq);
            }
            _ => {}
        }
        next += 1;
        let payload = serde_json::to_string(&event.kind).map_err(crate::error::corrupt)?;
        conn.execute(
            "INSERT INTO recipes_cook_event (session_id, seq, client_event_id, at, actor, type, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                row.id,
                next,
                event.client_event_id,
                event.at,
                actor_name(event.actor),
                kind_name(&event.kind),
                payload
            ],
        )
        .map_err(sql)?;
        if row.state == "active" {
            conn.execute(
                "UPDATE recipes_cook SET last_event_at = ?2 WHERE id = ?1",
                params![row.id, now],
            )
            .map_err(sql)?;
            row.last_event_at = now;
        }
        if let CookEventKind::Finish { note } = &event.kind {
            let (seq, done) = finish_tx(conn, now, &row, note.as_deref(), true, actor)?;
            library_seq = Some(seq);
            finished = done;
            row.state = "finished".to_owned();
            row.finished_at = Some(now);
        }
    }
    conn.execute(
        "UPDATE recipes_meta SET value = CAST(value AS INTEGER) + 1 WHERE key = 'cook_seq'",
        [],
    )
    .map_err(sql)?;
    if conn.changes() == 0 {
        crate::schema::set_meta(conn, "cook_seq", "1")?;
    }
    let row = session_row(conn, session_id)?
        .ok_or_else(|| not_found(format!("no cook session has id {session_id:?}")))?;
    Ok(Appended {
        session: wire_session(conn, &row)?,
        seq: next,
        library_seq,
        finished,
    })
}

/// What starting a session needs.
#[derive(Clone, Debug, PartialEq)]
pub struct Start {
    /// The client's id; a repeated start returns the existing session.
    pub id: Option<String>,
    pub recipe_id: String,
    pub revision: Option<u32>,
    pub scale: Option<f64>,
    pub servings: Option<f64>,
}

fn start_tx(conn: &Connection, now: i64, start: &Start) -> R<SessionRow> {
    if let Some(id) = &start.id {
        if !ids::is_id(id, ids::COOK) {
            return Err(invalid(
                "a session id is \"cook_\" plus 8 lowercase letters or digits",
            ));
        }
        if let Some(existing) = session_row(conn, id)? {
            return Ok(existing);
        }
    }
    let (recipe, _) = recipe_for_read(conn, &start.recipe_id)?;
    if recipe.state == "trashed" {
        return Err(not_found(format!("{} is in the trash", recipe.id)));
    }
    let revision = start.revision.unwrap_or(recipe.head_rev);
    let doc = doc_at(conn, &recipe.id, revision)?;
    let scale = factor(&doc, start.scale, start.servings)?;
    let row = SessionRow {
        id: start.id.clone().unwrap_or_else(|| ids::new_id(ids::COOK)),
        recipe_id: recipe.id,
        rev: revision,
        scale,
        state: "active".to_owned(),
        started_at: now,
        finished_at: None,
        last_event_at: now,
        note: None,
        photo_image: None,
    };
    conn.execute(
        "INSERT INTO recipes_cook (id, recipe_id, rev, scale, state, started_at, last_event_at)
         VALUES (?1, ?2, ?3, ?4, 'active', ?5, ?5)",
        params![row.id, row.recipe_id, row.rev, row.scale, now],
    )
    .map_err(sql)?;
    Ok(row)
}

/// What one pass of the timer service did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tick {
    /// Idle sessions closed.
    pub closed: u32,
    /// Timer alerts sent (or recorded, in record mode).
    pub alerts: u32,
}

struct DueTimer {
    session_id: String,
    step: String,
    ends_at: i64,
    title: String,
    action: String,
}

impl Recipes {
    async fn after_append(&self, appended: &Appended) {
        self.hub.session(&appended.session.id, appended.seq);
        if let Some(seq) = appended.library_seq {
            self.hub.library(seq);
        }
        if let Some(done) = &appended.finished {
            events::publish(
                &self.ports,
                events::cook_finished(
                    &done.session_id,
                    &done.recipe_id,
                    &done.title,
                    done.open_adjustments,
                    done.at,
                ),
            )
            .await;
        }
    }

    /// Starts a session, or returns the one `start.id` already names.
    pub async fn start_session(&self, start: Start) -> R<CookSession> {
        let session = self
            .transact(false, move |conn, now| {
                let row = start_tx(conn, now, &start)?;
                wire_session(conn, &row)
            })
            .await?;
        self.hub.session(&session.id, 0);
        Ok(session)
    }

    pub async fn session(&self, id: &str) -> R<CookSession> {
        let id = id.to_owned();
        self.query(move |conn| {
            let row = session_row(conn, &id)?
                .ok_or_else(|| not_found(format!("no cook session has id {id:?}")))?;
            wire_session(conn, &row)
        })
        .await
    }

    /// Appends events for the app or an agent.
    pub async fn append_events(
        &self,
        session_id: &str,
        events: Vec<Append>,
        strict: bool,
        actor: Actor,
    ) -> R<CookSession> {
        let id = session_id.to_owned();
        let appended = self
            .transact(false, move |conn, now| {
                append_tx(conn, now, &id, events, strict, &actor)
            })
            .await?;
        self.after_append(&appended).await;
        Ok(appended.session)
    }

    /// Stores the photo of a session's result as an image of its recipe.
    ///
    /// `upload_id` is the client's id for this upload: a repeat with the same
    /// id stores nothing and returns the session.
    pub async fn cook_photo(
        &self,
        session_id: &str,
        upload_id: Option<String>,
        bytes: Vec<u8>,
    ) -> R<CookSession> {
        self.ready().await?;
        if bytes.len() > crate::inbox::MAX_PHOTO_BYTES {
            return Err(reject("too_large", "a photo may be at most 25 MB"));
        }
        let held = self.files_lock.lock().await;
        let stored = self.files.put(bytes).await?;
        let id = session_id.to_owned();
        let (session, seq) = self
            .transact(false, move |conn, now| {
                let row = session_row(conn, &id)?
                    .ok_or_else(|| not_found(format!("no cook session has id {id:?}")))?;
                if let Some(upload) = &upload_id {
                    let key = format!("photo:{upload}");
                    let seen: i64 = conn
                        .query_row(
                            "SELECT count(*) FROM recipes_client_op WHERE id = ?1",
                            params![key],
                            |row| row.get(0),
                        )
                        .map_err(sql)?;
                    if seen > 0 {
                        return Ok((wire_session(conn, &row)?, None));
                    }
                    conn.execute(
                        "INSERT INTO recipes_client_op (id, applied_at, result) VALUES (?1, ?2, '{}')",
                        params![key, now],
                    )
                    .map_err(sql)?;
                }
                let ids = [row.recipe_id.clone()];
                let before = json!({ "recipes": meta::recipes_facets(conn, &ids, &[IMAGES])? });
                if let Some(previous) = &row.photo_image {
                    conn.execute("DELETE FROM recipes_image WHERE id = ?1", params![previous])
                        .map_err(sql)?;
                }
                let position =
                    u32::try_from(db::images_of(conn, &row.recipe_id)?.len()).unwrap_or(u32::MAX);
                let image = ImageRow {
                    id: ids::new_id(ids::IMAGE),
                    recipe_id: row.recipe_id.clone(),
                    sha256: stored.sha256,
                    mime: stored.mime,
                    width: stored.width,
                    height: stored.height,
                    bytes: stored.bytes,
                    role: "dish".to_owned(),
                    caption: None,
                    origin: format!("cook:{}", row.id),
                    color: stored.color,
                    position,
                    created_at: now,
                };
                db::insert_image(conn, &image)?;
                conn.execute(
                    "UPDATE recipes_cook SET photo_image = ?2 WHERE id = ?1",
                    params![row.id, image.id],
                )
                .map_err(sql)?;
                let seq = next_seq(conn)?;
                db::touch(conn, &row.recipe_id, seq, now)?;
                let after = json!({ "recipes": meta::recipes_facets(conn, &ids, &[IMAGES])? });
                log_change(
                    conn,
                    seq,
                    now,
                    &Change {
                        actor: &Actor::app(),
                        summary: "Added a photo from a cook".to_owned(),
                        kind: "meta",
                        touched: json!({ row.recipe_id.clone(): [IMAGES] }),
                        before,
                        after,
                    },
                )?;
                Ok((wire_session(conn, &row)?, Some(seq)))
            })
            .await?;
        drop(held);
        if let Some(seq) = seq {
            self.hub.library(seq);
        }
        Ok(session)
    }

    pub async fn cook_start(&self, input: CookStartInput) -> R<CookStartOutput> {
        let session = self
            .start_session(Start {
                id: None,
                recipe_id: input.recipe_id,
                revision: None,
                scale: input.scale,
                servings: input.servings,
            })
            .await?;
        Ok(CookStartOutput {
            app_url: self
                .app_url
                .as_ref()
                .map(|origin| format!("{origin}/cook/{}", session.id)),
            session_id: session.id,
            recipe_id: session.recipe_id,
            revision: session.revision,
            scale: session.scale,
        })
    }

    pub async fn cook_get(&self, input: CookGetInput) -> R<CookGetOutput> {
        let tz = self.tz.clone();
        let now = self.now();
        self.query(move |conn| {
            if let Some(id) = &input.session_id {
                let row = session_row(conn, id)?
                    .ok_or_else(|| not_found(format!("no cook session has id {id:?}")))?;
                return Ok(CookGetOutput {
                    session: Some(view(conn, &row, now)?),
                    ..CookGetOutput::default()
                });
            }
            if let Some(id) = &input.recipe_id {
                let (recipe, _) = recipe_for_read(conn, id)?;
                let mut cooks = Vec::new();
                for row in
                    sessions_where(conn, "state = 'finished' AND recipe_id = ?1", &recipe.id)?
                        .into_iter()
                        .rev()
                {
                    let projection = project(&session_events(conn, &row.id)?);
                    cooks.push(CookLogEntry {
                        on: db::local_date(row.finished_at.unwrap_or(row.started_at), &tz),
                        scale: row.scale,
                        note: row.note.clone(),
                        adjustments: projection
                            .adjustments
                            .into_iter()
                            .map(|(target, text)| SessionAdjustment { target, text })
                            .collect(),
                        session_id: row.id,
                    });
                }
                return Ok(CookGetOutput {
                    cooks: Some(cooks),
                    open_adjustments: Some(adjustment_infos(
                        &open_adjustments(conn, &recipe.id)?,
                        &tz,
                    )),
                    ..CookGetOutput::default()
                });
            }
            let mut active = Vec::new();
            for row in sessions_where(conn, "state = ?1", "active")? {
                let Ok(doc) = doc_at(conn, &row.recipe_id, row.rev) else {
                    continue;
                };
                let projection = project(&session_events(conn, &row.id)?);
                active.push(ActiveCook {
                    recipe_id: row.recipe_id.clone(),
                    title: doc.title.clone(),
                    done: u32::try_from(done_steps(&doc, &projection).len()).unwrap_or(0),
                    of: u32::try_from(step_ids(&doc).len()).unwrap_or(0),
                    last_event_at: iso(row.last_event_at),
                    session_id: row.id,
                });
            }
            Ok(CookGetOutput {
                active: Some(active),
                ..CookGetOutput::default()
            })
        })
        .await
    }

    pub async fn cook_update(&self, input: CookUpdateInput, actor: Actor) -> R<CookUpdateOutput> {
        let now = self.now();
        let events = input
            .events
            .into_iter()
            .map(|event| {
                let kind = match event {
                    CookEventInput::Check { target } => CookEventKind::Check { target },
                    CookEventInput::Uncheck { target } => CookEventKind::Uncheck { target },
                    CookEventInput::Adjust { target, text } => {
                        CookEventKind::Adjust { target, text }
                    }
                    CookEventInput::Unadjust { target } => CookEventKind::Unadjust { target },
                    CookEventInput::Note { text } => CookEventKind::Note { text },
                    CookEventInput::Timer { step, minutes } => {
                        if !(minutes.is_finite() && minutes > 0.0 && minutes <= 24.0 * 60.0) {
                            return Err(invalid("minutes must be above 0 and at most 1440"));
                        }
                        #[allow(clippy::cast_possible_truncation)]
                        let length = (minutes * 60_000.0).round() as i64;
                        CookEventKind::Timer {
                            step,
                            ends_at: now + length,
                        }
                    }
                    CookEventInput::CancelTimer { step } => CookEventKind::CancelTimer { step },
                    CookEventInput::Rebase { revision } => CookEventKind::Rebase { revision },
                    CookEventInput::Finish { note } => CookEventKind::Finish { note },
                    CookEventInput::DismissAdjustment { id } => {
                        CookEventKind::DismissAdjustment { id }
                    }
                };
                Ok(Append {
                    actor: CookActor::Agent,
                    client_event_id: format!("agent_{}", omni_core::ids::uuid_v4()),
                    at: now,
                    kind,
                })
            })
            .collect::<R<Vec<_>>>()?;
        let session_id = input.session_id.clone();
        self.append_events(&input.session_id, events, true, actor)
            .await?;
        let session = self
            .query(move |conn| {
                let row = session_row(conn, &session_id)?
                    .ok_or_else(|| not_found(format!("no cook session has id {session_id:?}")))?;
                view(conn, &row, now)
            })
            .await?;
        Ok(CookUpdateOutput { session })
    }

    /// Closes sessions idle for 18 hours and announces timers that ended
    /// while no app client was connected. The timer service calls it every
    /// few seconds; it is safe to call at any time.
    pub async fn tick(&self) -> R<Tick> {
        let mut tick = Tick::default();
        // Nothing is cooking most of the time: one read, no write transaction.
        let active: u32 = self
            .query(|conn| {
                conn.query_row(
                    "SELECT count(*) FROM recipes_cook WHERE state = 'active'",
                    [],
                    |row| row.get(0),
                )
                .map_err(sql)
            })
            .await?;
        if active == 0 {
            return Ok(tick);
        }
        let closed = self
            .transact(false, |conn, now| {
                let mut closed = Vec::new();
                for row in sessions_where(conn, "state = ?1", "active")? {
                    if now - row.last_event_at < IDLE_CLOSE_MS {
                        continue;
                    }
                    let counts = match doc_at(conn, &row.recipe_id, row.rev) {
                        Ok(doc) => {
                            let projection = project(&session_events(conn, &row.id)?);
                            let (done, of) =
                                (done_steps(&doc, &projection).len(), step_ids(&doc).len());
                            of > 0 && done * 2 >= of
                        }
                        Err(_) => false,
                    };
                    let seq: u32 = conn
                        .query_row(
                            "SELECT coalesce(max(seq), 0) + 1 FROM recipes_cook_event WHERE session_id = ?1",
                            params![row.id],
                            |row| row.get(0),
                        )
                        .map_err(sql)?;
                    let kind = CookEventKind::Finish { note: None };
                    conn.execute(
                        "INSERT OR IGNORE INTO recipes_cook_event
                           (session_id, seq, client_event_id, at, actor, type, payload)
                         VALUES (?1, ?2, ?3, ?4, 'system', 'finish', ?5)",
                        params![
                            row.id,
                            seq,
                            format!("system_idle_{}", row.id),
                            now,
                            serde_json::to_string(&kind).map_err(crate::error::corrupt)?
                        ],
                    )
                    .map_err(sql)?;
                    let (library_seq, finished) =
                        finish_tx(conn, now, &row, None, counts, &Actor::system())?;
                    closed.push((row.id, seq, library_seq, finished));
                }
                Ok(closed)
            })
            .await?;
        for (id, seq, library_seq, finished) in closed {
            tick.closed += 1;
            self.hub.session(&id, seq);
            self.hub.library(library_seq);
            if let Some(done) = finished {
                events::publish(
                    &self.ports,
                    events::cook_finished(
                        &done.session_id,
                        &done.recipe_id,
                        &done.title,
                        done.open_adjustments,
                        done.at,
                    ),
                )
                .await;
            }
        }

        // Reserve every ended timer in one transaction, then send outside it.
        let watched = self.hub.clients() > 0;
        let to_send = self
            .transact(false, move |conn, now| {
                let mut due: Vec<DueTimer> = Vec::new();
                for row in sessions_where(conn, "state = ?1", "active")? {
                    let projection = project(&session_events(conn, &row.id)?);
                    for (step, ends_at) in projection.timers {
                        if ends_at > now {
                            continue;
                        }
                        // A connected app rings by itself and cancels the timer
                        // when the cook answers; a phone that went to sleep with
                        // the stream still open never does, so after the grace
                        // period an unanswered timer is announced anyway.
                        if watched && now - ends_at < APP_GRACE_MS {
                            continue;
                        }
                        let state = if now - ends_at > ALERT_WINDOW_MS {
                            "missed"
                        } else {
                            "sending"
                        };
                        let reserved = conn
                            .execute(
                                "INSERT OR IGNORE INTO recipes_timer_alert (session_id, step, ends_at, state, at)
                                 VALUES (?1, ?2, ?3, ?4, ?5)",
                                params![row.id, step, ends_at, state, now],
                            )
                            .map_err(sql)?;
                        if reserved == 1 && state == "sending" {
                            let doc = doc_at(conn, &row.recipe_id, row.rev).ok();
                            let action = doc
                                .as_ref()
                                .and_then(|doc| {
                                    Index::build(doc).step(&step).map(|step| step.action.clone())
                                })
                                .unwrap_or_else(|| step.clone());
                            due.push(DueTimer {
                                session_id: row.id.clone(),
                                step,
                                ends_at,
                                title: doc.map(|doc| doc.title).unwrap_or_default(),
                                action,
                            });
                        }
                    }
                }
                Ok(due)
            })
            .await?;
        for timer in to_send {
            let outcome = match &self.online {
                Some(online) => {
                    let message = PushoverMessage {
                        title: Some("Timer done".to_owned()),
                        message: format!("{}: {}", timer.title, timer.action),
                        url: self
                            .app_url
                            .as_ref()
                            .map(|origin| format!("{origin}/cook/{}", timer.session_id)),
                        url_title: Some("Open in Fond".to_owned()),
                        thread_id: Some("recipes".to_owned()),
                        ..PushoverMessage::default()
                    };
                    match online
                        .pushover
                        .send(PushoverChannel::General, message)
                        .await
                    {
                        Ok(PushOutcome::SkippedNoToken | PushOutcome::Disabled) => "unrouted",
                        Ok(_) => {
                            tick.alerts += 1;
                            "sent"
                        }
                        Err(error) => {
                            // Uncertain or refused: the reservation stands, so it is never resent.
                            tracing::warn!(target: LOG, error = %error, "Cook timer alert not delivered");
                            "failed"
                        }
                    }
                }
                None => "unrouted",
            };
            self.transact(false, move |conn, now| {
                conn.execute(
                    "UPDATE recipes_timer_alert SET state = ?4, at = ?5
                     WHERE session_id = ?1 AND step = ?2 AND ends_at = ?3",
                    params![timer.session_id, timer.step, timer.ends_at, outcome, now],
                )
                .map_err(sql)?;
                Ok(())
            })
            .await?;
        }
        Ok(tick)
    }
}
