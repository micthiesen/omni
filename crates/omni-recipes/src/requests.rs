//! Requests for the kitchen agent ("Ask Kitchen" in the app).
//!
//! A request is a row of `recipes_request`: the text the cook wrote and what
//! they were looking at. It is stored before anything else happens, so the
//! library is the source of truth: the notice that announces it
//! (`recipe.request`, handed to the event journal for the stored `kitchen`
//! target, which sends and retries it) may fail, and an agent finds every
//! request with `recipes_requests_list` whether or not the notice arrived.
//! The agent answers with ordinary recipe revisions and records the outcome
//! with `recipes_request_update`.

use std::sync::Arc;
use std::time::Duration;

use omni_api::fond::{self, KitchenRequest, RequestList, RequestOrigin};
use omni_recipe_format::Index;
use omni_runtime::ports::{EventPublisher, NoticeStatus, PortError, TargetNotice};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::{Value, json};

use crate::api::{
    self, RequestInfo, RequestStatus, RequestStatusFilter, RequestUpdateInput, RequestUpdateOutput,
    RequestsListInput, RequestsListOutput,
};
use crate::db::{self, iso};
use crate::error::{R, invalid, not_found, sql};
use crate::ids;
use crate::schema::{meta_u64, set_meta};
use crate::service::Recipes;

const LOG: &str = "Recipes";

/// The webhook event name.
pub const EVENT: &str = "recipe.request";
/// Characters a request holds.
pub const MAX_TEXT: usize = 2000;
/// Characters the agent's note holds.
pub const MAX_NOTE: usize = 300;
/// Requests waiting or being worked on at once; the app has no login.
pub const MAX_OPEN: u32 = 100;
/// Requests the app's list returns.
const APP_LIST: u32 = 200;
/// How often the hand-over service looks for owed notices when nothing wakes it.
pub const DELIVER_EVERY: Duration = Duration::from_secs(15);
const MAX_DUE_PER_PASS: u32 = 10;

/// Where a request's notice stands (the journal's notice state, once the
/// notice is handed over).
pub mod delivery {
    /// Owed to the webhook (not handed over, or waiting in the journal).
    pub const PENDING: &str = "pending";
    pub const DELIVERED: &str = "delivered";
    /// Accepted, but the receiving bot ran no webhook routine.
    pub const IGNORED: &str = "ignored";
    /// Refused for good, or still failing a day later.
    pub const FAILED: &str = "failed";
    /// No webhook was configured when the request was written.
    pub const UNCONFIGURED: &str = "unconfigured";
    /// An agent took the request before the notice went out.
    pub const SKIPPED: &str = "skipped";
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RequestRow {
    pub id: String,
    pub text: String,
    pub origin: String,
    pub recipe_id: Option<String>,
    pub revision: Option<u32>,
    pub scale: Option<f64>,
    pub step: Option<String>,
    pub status: String,
    pub note: Option<String>,
    pub result_recipe_id: Option<String>,
    pub result_revision: Option<u32>,
    pub created_at: i64,
    pub updated_at: i64,
    pub delivery: String,
    pub attempts: u32,
    pub next_attempt_at: i64,
    pub last_error: Option<String>,
}

const COLUMNS: &str = "id, text, origin, recipe_id, revision, scale, step, status, note, \
     result_recipe_id, result_revision, created_at, updated_at, delivery, attempts, \
     next_attempt_at, last_error";

fn from_row(row: &Row<'_>) -> rusqlite::Result<RequestRow> {
    Ok(RequestRow {
        id: row.get(0)?,
        text: row.get(1)?,
        origin: row.get(2)?,
        recipe_id: row.get(3)?,
        revision: row.get(4)?,
        scale: row.get(5)?,
        step: row.get(6)?,
        status: row.get(7)?,
        note: row.get(8)?,
        result_recipe_id: row.get(9)?,
        result_revision: row.get(10)?,
        created_at: row.get(11)?,
        updated_at: row.get(12)?,
        delivery: row.get(13)?,
        attempts: row.get(14)?,
        next_attempt_at: row.get(15)?,
        last_error: row.get(16)?,
    })
}

fn get(conn: &Connection, id: &str) -> R<Option<RequestRow>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM recipes_request WHERE id = ?1"),
        params![id],
        from_row,
    )
    .optional()
    .map_err(sql)
}

fn status_of(text: &str) -> RequestStatus {
    match text {
        "working" => RequestStatus::Working,
        "done" => RequestStatus::Done,
        "declined" => RequestStatus::Declined,
        _ => RequestStatus::Queued,
    }
}

fn origin_of(text: &str) -> RequestOrigin {
    match text {
        "recipe" => RequestOrigin::Recipe,
        "cook" => RequestOrigin::Cook,
        _ => RequestOrigin::Library,
    }
}

fn wire_status(text: &str) -> fond::RequestStatus {
    match text {
        "working" => fond::RequestStatus::Working,
        "done" => fond::RequestStatus::Done,
        "declined" => fond::RequestStatus::Declined,
        _ => fond::RequestStatus::Queued,
    }
}

fn tool_origin(text: &str) -> api::RequestOrigin {
    match text {
        "recipe" => api::RequestOrigin::Recipe,
        "cook" => api::RequestOrigin::Cook,
        _ => api::RequestOrigin::Library,
    }
}

/// Request writes do not change the library the app syncs, so they count on
/// the same counter as cook events: the one the hourly archive also watches.
fn bump(conn: &Connection) -> R<()> {
    let next = meta_u64(conn, "cook_seq")? + 1;
    set_meta(conn, "cook_seq", &next.to_string())
}

/// The title of a request's recipe as it is now.
fn title_of(conn: &Connection, recipe_id: Option<&str>) -> R<Option<String>> {
    match recipe_id {
        Some(id) => Ok(db::recipe(conn, id)?.map(|row| row.title)),
        None => Ok(None),
    }
}

/// What the app sends with a request.
pub(crate) struct Submit<'a> {
    pub id: &'a str,
    pub text: &'a str,
    pub origin: RequestOrigin,
    pub recipe_id: Option<&'a str>,
    pub revision: Option<u32>,
    pub scale: Option<f64>,
    pub step: Option<&'a str>,
}

/// Stores a request once by its id. `Ok(Ok(true))` when it is new,
/// `Ok(Ok(false))` when that id is already stored, and `Ok(Err(line))` with
/// one line for the cook when it can never be stored.
pub(crate) fn submit(conn: &Connection, now: i64, submit: &Submit<'_>) -> R<Result<bool, String>> {
    let refuse = |message: &str| Ok(Err(message.to_owned()));
    if !ids::is_id(submit.id, ids::REQUEST) {
        return refuse("the request has no valid id");
    }
    if get(conn, submit.id)?.is_some() {
        return Ok(Ok(false));
    }
    let text = submit.text.trim();
    if text.is_empty() {
        return refuse("the request is empty");
    }
    if text.chars().count() > MAX_TEXT {
        return refuse("a request is limited to 2,000 characters");
    }
    let open: u32 = conn
        .query_row(
            "SELECT count(*) FROM recipes_request WHERE status IN ('queued', 'working')",
            [],
            |row| row.get(0),
        )
        .map_err(sql)?;
    if open >= MAX_OPEN {
        return refuse("too many requests are already waiting for Kitchen");
    }
    // What the cook was looking at. A recipe that was merged away is named
    // by the recipe it lives under. Context that does not check out is
    // dropped rather than refusing the request over it: a request written
    // offline about a recipe that has since gone is kept as a library
    // request, so the text is never lost.
    let recipe = match (submit.origin, submit.recipe_id) {
        (RequestOrigin::Library, _) | (_, None) => None,
        (_, Some(id)) => match db::recipe_for_read(conn, id) {
            Ok((row, _)) => Some(row),
            Err(error) if error.is_rejection() => None,
            Err(error) => return Err(error),
        },
    };
    let revision = match (&recipe, submit.revision) {
        (Some(row), Some(revision)) => {
            db::revision(conn, &row.id, revision)?.map(|found| found.rev)
        }
        _ => None,
    };
    let scale = submit
        .scale
        .filter(|scale| scale.is_finite() && (0.05..=100.0).contains(scale))
        .filter(|_| recipe.is_some());
    let step = submit
        .step
        .filter(|step| ids::is_client_token(step))
        .filter(|_| recipe.is_some());
    conn.execute(
        "INSERT INTO recipes_request (id, text, origin, recipe_id, revision, scale, step, status,
           created_at, updated_at, delivery, attempts, next_attempt_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'queued', ?8, ?8, ?9, 0, ?8)",
        params![
            submit.id,
            text,
            if recipe.is_some() {
                submit.origin.as_str()
            } else {
                RequestOrigin::Library.as_str()
            },
            recipe.as_ref().map(|row| row.id.as_str()),
            revision,
            scale,
            step,
            now,
            delivery::PENDING,
        ],
    )
    .map_err(sql)?;
    bump(conn)?;
    Ok(Ok(true))
}

fn wire(conn: &Connection, row: RequestRow) -> R<KitchenRequest> {
    Ok(KitchenRequest {
        recipe_title: title_of(conn, row.recipe_id.as_deref())?,
        status: wire_status(&row.status),
        origin: origin_of(&row.origin),
        id: row.id,
        text: row.text,
        at: row.created_at,
        updated_at: row.updated_at,
        recipe_id: row.recipe_id,
        note: row.note,
        result_recipe_id: row.result_recipe_id,
        result_revision: row.result_revision,
    })
}

fn info(conn: &Connection, row: RequestRow) -> R<RequestInfo> {
    let recipe = match &row.recipe_id {
        Some(id) => db::recipe(conn, id)?,
        None => None,
    };
    // The step as the cook saw it, from the revision they were looking at.
    let step_action = match (&row.recipe_id, row.revision, &row.step) {
        (Some(id), Some(revision), Some(step)) => {
            db::doc_at(conn, id, revision).ok().and_then(|doc| {
                Index::build(&doc)
                    .step(step)
                    .map(|step| step.action.clone())
            })
        }
        _ => None,
    };
    Ok(RequestInfo {
        status: status_of(&row.status),
        origin: tool_origin(&row.origin),
        recipe_title: recipe.as_ref().map(|recipe| recipe.title.clone()),
        head_revision: recipe.as_ref().map(|recipe| recipe.head_rev),
        step_action,
        request_id: row.id,
        text: row.text,
        created_at: iso(row.created_at),
        updated_at: iso(row.updated_at),
        recipe_id: row.recipe_id,
        revision: row.revision,
        scale: row.scale,
        step: row.step,
        note: row.note,
        result_recipe_id: row.result_recipe_id,
        result_revision: row.result_revision,
        delivery: row.delivery,
        delivery_attempts: row.attempts,
    })
}

fn update(conn: &Connection, now: i64, input: &RequestUpdateInput) -> R<(bool, RequestInfo)> {
    let mut row = get(conn, &input.request_id)?
        .ok_or_else(|| not_found(format!("no request has id {:?}", input.request_id)))?;
    if input.status.is_none()
        && input.note.is_none()
        && input.result_revision.is_none()
        && input.result_recipe_id.is_none()
    {
        return Err(invalid(
            "pass at least one of status, note, resultRevision and resultRecipeId",
        ));
    }
    let before = row.clone();
    if let Some(status) = input.status {
        row.status = status.as_str().to_owned();
    }
    if let Some(note) = &input.note {
        let note = note.trim();
        if note.chars().count() > MAX_NOTE {
            return Err(invalid("note is limited to 300 characters"));
        }
        row.note = (!note.is_empty()).then(|| note.to_owned());
    }
    if input.result_revision.is_some() || input.result_recipe_id.is_some() {
        let named = input
            .result_recipe_id
            .clone()
            .or_else(|| row.result_recipe_id.clone())
            .or_else(|| row.recipe_id.clone())
            .ok_or_else(|| {
                invalid(
                    "this request names no recipe; pass resultRecipeId with the recipe that answers it",
                )
            })?;
        let (recipe, _) = db::recipe_for_read(conn, &named)?;
        // Without a revision, the recipe as it is now answers the request.
        let revision = input.result_revision.unwrap_or(recipe.head_rev);
        if db::revision(conn, &recipe.id, revision)?.is_none() {
            return Err(not_found(format!(
                "{} has no revision {revision}",
                recipe.id
            )));
        }
        row.result_recipe_id = Some(recipe.id);
        row.result_revision = Some(revision);
    }
    let changed = row != before;
    if changed {
        row.updated_at = now;
        conn.execute(
            "UPDATE recipes_request SET status = ?2, note = ?3, result_recipe_id = ?4,
               result_revision = ?5, updated_at = ?6 WHERE id = ?1",
            params![
                row.id,
                row.status,
                row.note,
                row.result_recipe_id,
                row.result_revision,
                now
            ],
        )
        .map_err(sql)?;
        bump(conn)?;
    }
    Ok((changed, info(conn, row)?))
}

/// The JSON body of a request's notice.
fn body(conn: &Connection, row: &RequestRow, app_url: Option<&str>) -> R<Value> {
    let title = title_of(conn, row.recipe_id.as_deref())?;
    let link = app_url.map(|origin| match &row.recipe_id {
        Some(id) => format!("{origin}/r/{id}"),
        None => format!("{origin}/requests"),
    });
    Ok(json!({
        "event": EVENT,
        "requestId": row.id,
        "text": row.text,
        "createdAt": iso(row.created_at),
        "recipeTitle": title,
        "link": link,
        "context": {
            "origin": row.origin,
            "recipeId": row.recipe_id,
            "revision": row.revision,
            "scale": row.scale,
            "step": row.step,
        },
    }))
}

// ---------------------------------------------------------------------------
// Delivery: the event journal

/// The stored event target notices go to (imported by `omni-mcp` from the
/// old `RECIPES_REQUEST_WEBHOOK_*` settings under this id).
pub const KITCHEN_TARGET: &str = "kitchen";
/// The `delivery` column of a request handed to the event journal; its
/// notice's state is read from there.
const JOURNAL: &str = "journal";
/// How long a request waits before the journal is asked again after it
/// could not take the notice.
const HANDOVER_RETRY_MS: i64 = 30_000;

/// The wake-up of the service that hands notices to the journal.
#[derive(Clone, Default)]
pub struct RequestDelivery {
    pub(crate) wake: Arc<tokio::sync::Notify>,
}

impl RequestDelivery {
    pub fn none() -> Self {
        Self::default()
    }
}

fn settle(conn: &Connection, id: &str, state: &str, error: Option<&str>) -> R<()> {
    conn.execute(
        "UPDATE recipes_request SET delivery = ?2, last_error = ?3 WHERE id = ?1",
        params![id, state, error],
    )
    .map_err(sql)?;
    Ok(())
}

/// The tool's `delivery` word for a journal notice state.
fn notice_word(status: NoticeStatus) -> &'static str {
    match status {
        NoticeStatus::Pending => delivery::PENDING,
        NoticeStatus::Delivered => delivery::DELIVERED,
        NoticeStatus::Ignored => delivery::IGNORED,
        NoticeStatus::Failed => delivery::FAILED,
        NoticeStatus::Unconfigured => delivery::UNCONFIGURED,
        NoticeStatus::Skipped => delivery::SKIPPED,
    }
}

impl Recipes {
    /// The newest requests, for the app's Requests page.
    pub async fn requests_for_app(&self) -> R<RequestList> {
        self.query(|conn| {
            let mut statement = conn
                .prepare(&format!(
                    "SELECT {COLUMNS} FROM recipes_request ORDER BY created_at DESC, id LIMIT ?1"
                ))
                .map_err(sql)?;
            let rows = statement
                .query_map(params![APP_LIST], from_row)
                .map_err(sql)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql)?;
            let mut items = Vec::with_capacity(rows.len());
            for row in rows {
                items.push(wire(conn, row)?);
            }
            Ok(RequestList { items })
        })
        .await
    }

    pub async fn requests_list(&self, input: RequestsListInput) -> R<RequestsListOutput> {
        let limit = input.limit.unwrap_or(25).clamp(1, 100);
        let cursor = input.cursor.unwrap_or(0);
        let statuses = match input.status.unwrap_or(RequestStatusFilter::Queued) {
            RequestStatusFilter::Queued => "'queued'",
            RequestStatusFilter::Working => "'working'",
            RequestStatusFilter::Done => "'done'",
            RequestStatusFilter::Declined => "'declined'",
            RequestStatusFilter::Open => "'queued', 'working'",
            RequestStatusFilter::All => "'queued', 'working', 'done', 'declined'",
        };
        let recipe_id = input.recipe_id;
        let mut output = self
            .query(move |conn| {
                // A recipe that was merged away is asked for by where it lives now.
                let recipe = match &recipe_id {
                    Some(id) => Some(db::recipe_for_read(conn, id)?.0.id),
                    None => None,
                };
                let filter = format!("status IN ({statuses}) AND (?1 IS NULL OR recipe_id = ?1)");
                let total: u32 = conn
                    .query_row(
                        &format!("SELECT count(*) FROM recipes_request WHERE {filter}"),
                        params![recipe],
                        |row| row.get(0),
                    )
                    .map_err(sql)?;
                // Oldest first: the order to work through them in.
                let mut statement = conn
                    .prepare(&format!(
                        "SELECT {COLUMNS} FROM recipes_request WHERE {filter}
                     ORDER BY created_at, id LIMIT ?2 OFFSET ?3"
                    ))
                    .map_err(sql)?;
                let rows = statement
                    .query_map(params![recipe, limit, cursor], from_row)
                    .map_err(sql)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(sql)?;
                let mut requests = Vec::with_capacity(rows.len());
                for row in rows {
                    requests.push(info(conn, row)?);
                }
                Ok(RequestsListOutput {
                    total,
                    next_cursor: (cursor.saturating_add(limit) < total)
                        .then_some(cursor.saturating_add(limit)),
                    requests,
                })
            })
            .await?;
        output.requests = self.with_notice_states(output.requests).await;
        Ok(output)
    }

    pub async fn request_update(&self, input: RequestUpdateInput) -> R<RequestUpdateOutput> {
        let (changed, request) = self
            .transact(false, move |conn, now| update(conn, now, &input))
            .await?;
        if changed {
            self.hub.requests();
            // An agent took the request: a notice not sent yet is not needed.
            if request.status != RequestStatus::Queued
                && let Some(journal) = self.ports.event_publisher()
                && let Err(error) = journal.cancel_notice(&request.request_id).await
            {
                tracing::info!(target: LOG, request = %request.request_id, %error, "Recipe request notice not cancelled");
            }
        }
        let request = self
            .with_notice_states(vec![request])
            .await
            .pop()
            .ok_or_else(|| invalid("the request disappeared"))?;
        Ok(RequestUpdateOutput { changed, request })
    }

    /// Tells the delivery service a request was just stored.
    pub(crate) fn request_added(&self) {
        self.hub.requests();
        self.requests.wake.notify_one();
    }

    /// Replaces `delivery: "journal"` with the notice's state in the event
    /// journal (pending when the journal cannot say).
    async fn with_notice_states(&self, mut infos: Vec<RequestInfo>) -> Vec<RequestInfo> {
        let keys: Vec<String> = infos
            .iter()
            .filter(|info| info.delivery == JOURNAL)
            .map(|info| info.request_id.clone())
            .collect();
        if keys.is_empty() {
            return infos;
        }
        let states = match self.ports.event_publisher() {
            Some(journal) => journal.notice_states(keys).await.unwrap_or_default(),
            None => Default::default(),
        };
        for info in &mut infos {
            if info.delivery != JOURNAL {
                continue;
            }
            match states.get(&info.request_id) {
                Some(state) => {
                    info.delivery = notice_word(state.status).to_owned();
                    info.delivery_attempts = state.attempts;
                }
                None => info.delivery = delivery::PENDING.to_owned(),
            }
        }
        infos
    }

    /// Hands the notice of every request still owed one to the event
    /// journal (a few per pass) and returns how many were handed over. The
    /// journal keys the notice by the request id, which is also its
    /// `Idempotency-Key`, so handing it over twice sends it once. A request
    /// an agent already took needs no notice.
    pub async fn deliver_requests(&self) -> R<usize> {
        let Some(journal) = self.ports.event_publisher() else {
            return Ok(0);
        };
        let now = self.now();
        let due = self
            .query(move |conn| {
                let mut statement = conn
                    .prepare(&format!(
                        "SELECT {COLUMNS} FROM recipes_request
                         WHERE delivery = 'pending' AND next_attempt_at <= ?1
                         ORDER BY next_attempt_at, id LIMIT ?2"
                    ))
                    .map_err(sql)?;
                let rows = statement
                    .query_map(params![now, MAX_DUE_PER_PASS], from_row)
                    .map_err(sql)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(sql)?;
                Ok(rows)
            })
            .await?;
        let mut handed = 0;
        for row in due {
            if row.status != "queued" {
                let id = row.id.clone();
                self.transact(false, move |conn, _now| {
                    settle(conn, &id, delivery::SKIPPED, None)
                })
                .await?;
                continue;
            }
            let app_url = self.app_url.clone();
            let reserve = row.clone();
            let payload = self
                .query(move |conn| body(conn, &reserve, app_url.as_deref()))
                .await?;
            let notice = TargetNotice {
                target: KITCHEN_TARGET.to_owned(),
                key: row.id.clone(),
                name: EVENT.to_owned(),
                body: payload,
            };
            match journal.notify_target(notice).await {
                Ok(()) => {
                    let id = row.id.clone();
                    self.transact(false, move |conn, _now| settle(conn, &id, JOURNAL, None))
                        .await?;
                    handed += 1;
                    tracing::info!(target: LOG, request = %row.id, "Recipe request notice handed to the event journal");
                    // An agent may have taken it while it was handed over.
                    let id = row.id.clone();
                    let taken = self
                        .query(
                            move |conn| Ok(get(conn, &id)?.is_some_and(|r| r.status != "queued")),
                        )
                        .await?;
                    if taken {
                        let _ = journal.cancel_notice(&row.id).await;
                    }
                }
                Err(PortError::Failed {
                    message,
                    transient: false,
                }) => {
                    // The journal refused this notice for good (too large).
                    let id = row.id.clone();
                    tracing::warn!(target: LOG, request = %row.id, error = %message, "Recipe request notice refused by the event journal; the request stays queued");
                    self.transact(false, move |conn, _now| {
                        settle(conn, &id, delivery::FAILED, Some(&message))
                    })
                    .await?;
                }
                Err(error) => {
                    let id = row.id.clone();
                    let message = error.to_string();
                    tracing::warn!(target: LOG, request = %row.id, error = %message, "Recipe request notice not handed over; will try again");
                    self.transact(false, move |conn, now| {
                        conn.execute(
                            "UPDATE recipes_request SET next_attempt_at = ?2, last_error = ?3 WHERE id = ?1",
                            params![id, now + HANDOVER_RETRY_MS, message],
                        )
                        .map_err(sql)?;
                        Ok(())
                    })
                    .await?;
                }
            }
        }
        self.write_back_settled(journal.as_ref()).await?;
        Ok(handed)
    }

    /// Copies settled notice states from the journal onto the requests, so
    /// a request keeps its notice's outcome after the journal forgets it
    /// (a week after it settled).
    async fn write_back_settled(&self, journal: &dyn EventPublisher) -> R<()> {
        let ids = self
            .query(|conn| {
                let mut statement = conn
                    .prepare(
                        "SELECT id FROM recipes_request WHERE delivery = 'journal'
                         ORDER BY updated_at LIMIT 50",
                    )
                    .map_err(sql)?;
                let ids = statement
                    .query_map([], |row| row.get::<_, String>(0))
                    .map_err(sql)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(sql)?;
                Ok(ids)
            })
            .await?;
        if ids.is_empty() {
            return Ok(());
        }
        let Ok(states) = journal.notice_states(ids).await else {
            return Ok(());
        };
        let settled: Vec<(String, &'static str, u32)> = states
            .into_iter()
            .filter(|(_, state)| {
                matches!(
                    state.status,
                    NoticeStatus::Delivered
                        | NoticeStatus::Ignored
                        | NoticeStatus::Failed
                        | NoticeStatus::Skipped
                )
            })
            .map(|(id, state)| (id, notice_word(state.status), state.attempts))
            .collect();
        if settled.is_empty() {
            return Ok(());
        }
        self.transact(false, move |conn, _now| {
            for (id, word, attempts) in &settled {
                conn.execute(
                    "UPDATE recipes_request SET delivery = ?2, attempts = ?3
                     WHERE id = ?1 AND delivery = 'journal'",
                    params![id, word, attempts],
                )
                .map_err(sql)?;
            }
            Ok(())
        })
        .await
    }

    /// Requests from before the journal that were never announced, and are
    /// still waiting for an agent, are owed a notice: the service queues them
    /// when it starts. Returns how many.
    pub async fn announce_unannounced(&self) -> R<usize> {
        if self.ports.event_publisher().is_none() {
            return Ok(0);
        }
        let queued = self
            .transact(false, |conn, now| {
                conn.execute(
                    "UPDATE recipes_request SET delivery = 'pending', next_attempt_at = ?1
                     WHERE delivery = 'unconfigured' AND status = 'queued'",
                    params![now],
                )
                .map_err(sql)
            })
            .await?;
        if queued > 0 {
            tracing::info!(target: LOG, "Queued {queued} recipe request(s) from before the event journal for a notice");
        }
        Ok(queued)
    }

    /// The hand-over service: runs when a request is stored and every few
    /// seconds, until shutdown. A failed pass is logged and the next one
    /// runs as usual.
    pub async fn run_requests(&self, shutdown: tokio_util::sync::CancellationToken) {
        if self.ports.event_publisher().is_none() {
            tracing::info!(
                target: LOG,
                "The event journal is unavailable; requests stay in the library for recipes_requests_list"
            );
        }
        if let Err(error) = self.announce_unannounced().await {
            tracing::warn!(target: LOG, "Recipe requests could not be queued for a notice: {}", error.tool_text());
        }
        let mut ticker = tokio::time::interval(DELIVER_EVERY);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = shutdown.cancelled() => return,
                () = self.requests.wake.notified() => {}
                _ = ticker.tick() => {}
            }
            if let Err(error) = self.deliver_requests().await {
                tracing::warn!(target: LOG, "Recipe request notice pass failed: {}", error.tool_text());
            }
        }
    }
}
