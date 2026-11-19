//! Requests for the kitchen agent ("Ask Kitchen" in the app).
//!
//! A request is a row of `recipes_request`: the text the cook wrote and what
//! they were looking at. It is stored before anything else happens, so the
//! library is the source of truth: the webhook that announces it
//! (`recipe.request`) is a notice that may fail, and an agent finds every
//! request with `recipes_requests_list` whether or not the notice arrived.
//! The agent answers with ordinary recipe revisions and records the outcome
//! with `recipes_request_update`.

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use omni_api::fond::{self, KitchenRequest, RequestList, RequestOrigin};
use omni_config::ClaudeNotifyTarget;
use omni_http::{HttpClient, HttpConfig, HttpError, Method, RedirectRule, SideEffectMode};
use omni_recipe_format::Index;
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
/// How often the delivery service looks for due notices when nothing wakes it.
pub const DELIVER_EVERY: Duration = Duration::from_secs(15);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// A webhook answers with a few bytes; anything longer is some other page.
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const FIRST_BACKOFF_MS: i64 = 30_000;
const MAX_BACKOFF_MS: i64 = 30 * 60_000;
/// A notice that still fails this long after the request was written is
/// given up; the request stays queued for a sweep.
pub const GIVE_UP_AFTER_MS: i64 = 24 * 60 * 60_000;
const MAX_DUE_PER_PASS: u32 = 10;

/// Where a request's notice stands.
pub mod delivery {
    /// Owed to the webhook.
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
pub(crate) fn submit(
    conn: &Connection,
    now: i64,
    submit: &Submit<'_>,
    webhook: bool,
) -> R<Result<bool, String>> {
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
            if webhook {
                delivery::PENDING
            } else {
                delivery::UNCONFIGURED
            },
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
// Delivery

/// One answer from the webhook.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebhookResponse {
    pub status: u16,
    /// `{"ok": true, "ignored": true}`: accepted, but no routine ran.
    pub ignored: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WebhookError {
    #[error("timeout")]
    Timeout,
    #[error("network")]
    Network,
    /// An answer arrived that is not a webhook's (too long to be one), or
    /// the request could not be made at all. Sending again would not help.
    #[error("unusable_answer")]
    Unusable,
}

/// The network leg; tests replace it.
pub trait RequestTransport: Send + Sync {
    fn post<'a>(
        &'a self,
        target: &'a ClaudeNotifyTarget,
        idempotency_key: &'a str,
        body: &'a str,
    ) -> BoxFuture<'a, Result<WebhookResponse, WebhookError>>;
}

/// The production transport: the LAN-capable client (the address comes from
/// configuration, never from the app), no redirects, a ten-second timeout. In
/// `SideEffectMode::Record` nothing is sent.
pub struct LiveTransport {
    client: HttpClient,
    mode: SideEffectMode,
}

impl LiveTransport {
    pub fn new(client: HttpClient, mode: SideEffectMode) -> Self {
        Self { client, mode }
    }
}

impl RequestTransport for LiveTransport {
    fn post<'a>(
        &'a self,
        target: &'a ClaudeNotifyTarget,
        idempotency_key: &'a str,
        body: &'a str,
    ) -> BoxFuture<'a, Result<WebhookResponse, WebhookError>> {
        Box::pin(async move {
            if self.mode == SideEffectMode::Record {
                tracing::info!(
                    target: LOG,
                    host = target.url.host_str().unwrap_or(""),
                    "Recorded recipe request notice (not sent)"
                );
                return Ok(WebhookResponse {
                    status: 200,
                    ignored: false,
                });
            }
            let pinned;
            let client = match &target.connect {
                None => &self.client,
                Some(connect) => {
                    let host = target.url.host_str().unwrap_or_default();
                    let port = target.url.port_or_known_default().unwrap_or(443);
                    // Resolved per request, so a recreated proxy container's
                    // new address is picked up.
                    let addr = tokio::net::lookup_host((connect.as_str(), port))
                        .await
                        .ok()
                        .and_then(|mut addrs| addrs.next())
                        .ok_or(WebhookError::Network)?;
                    let config = HttpConfig {
                        connect_timeout: Some(REQUEST_TIMEOUT),
                        offline: false,
                    };
                    pinned = HttpClient::pinned(&config, host, addr)
                        .map_err(|_| WebhookError::Network)?;
                    &pinned
                }
            };
            let mut request = client
                .request(Method::POST, target.url.clone())
                .redirect(RedirectRule::None)
                .timeout(REQUEST_TIMEOUT)
                .header("content-type", "application/json")
                .header("idempotency-key", idempotency_key)
                .header("x-fond-event", EVENT);
            if !target.token.is_empty() {
                request = request.bearer_auth(&target.token);
            }
            let response = request
                .body(body.to_owned())
                .send_bounded(MAX_RESPONSE_BYTES)
                .await
                .map_err(|error| match error {
                    HttpError::Timeout => WebhookError::Timeout,
                    HttpError::Network(_) => WebhookError::Network,
                    _ => WebhookError::Unusable,
                })?;
            let ignored = serde_json::from_slice::<Value>(&response.body)
                .ok()
                .and_then(|value| value.get("ignored").and_then(Value::as_bool))
                .unwrap_or(false);
            Ok(WebhookResponse {
                status: response.status.as_u16(),
                ignored,
            })
        })
    }
}

/// The transport of a service without a webhook; it is never called.
struct NoTransport;

impl RequestTransport for NoTransport {
    fn post<'a>(
        &'a self,
        _target: &'a ClaudeNotifyTarget,
        _idempotency_key: &'a str,
        _body: &'a str,
    ) -> BoxFuture<'a, Result<WebhookResponse, WebhookError>> {
        Box::pin(async { Err(WebhookError::Network) })
    }
}

/// Where request notices go, and the wake-up of the service that sends them.
#[derive(Clone)]
pub struct RequestDelivery {
    pub(crate) target: Option<Arc<ClaudeNotifyTarget>>,
    pub(crate) transport: Arc<dyn RequestTransport>,
    pub(crate) wake: Arc<tokio::sync::Notify>,
}

impl RequestDelivery {
    pub fn none() -> Self {
        Self {
            target: None,
            transport: Arc::new(NoTransport),
            wake: Arc::new(tokio::sync::Notify::new()),
        }
    }

    pub fn new(target: Option<ClaudeNotifyTarget>, transport: Arc<dyn RequestTransport>) -> Self {
        Self {
            target: target.map(Arc::new),
            transport,
            wake: Arc::new(tokio::sync::Notify::new()),
        }
    }
}

/// How one attempt ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Attempt {
    Delivered,
    Ignored,
    /// Worth another attempt: a timeout, a network failure, 408, 425, 429 or 5xx.
    Retry(String),
    /// Refused for good: another 4xx, or a redirect that is never followed.
    Rejected(String),
}

/// The pure decision over one webhook answer.
pub fn classify(result: &Result<WebhookResponse, WebhookError>) -> Attempt {
    match result {
        Err(WebhookError::Unusable) => Attempt::Rejected(WebhookError::Unusable.to_string()),
        Err(error) => Attempt::Retry(error.to_string()),
        Ok(response) if (200..300).contains(&response.status) => {
            if response.ignored {
                Attempt::Ignored
            } else {
                Attempt::Delivered
            }
        }
        Ok(response) if matches!(response.status, 408 | 425 | 429) || response.status >= 500 => {
            Attempt::Retry(format!("http_{}", response.status))
        }
        Ok(response) => Attempt::Rejected(format!("http_{}", response.status)),
    }
}

/// The wait before attempt `attempts + 1`: 30 seconds doubling to 30 minutes.
pub fn backoff_ms(attempts: u32) -> i64 {
    FIRST_BACKOFF_MS
        .saturating_mul(1_i64 << attempts.saturating_sub(1).min(16))
        .min(MAX_BACKOFF_MS)
}

fn settle(conn: &Connection, id: &str, state: &str, error: Option<&str>, now: i64) -> R<()> {
    conn.execute(
        "UPDATE recipes_request SET delivery = ?2, last_error = ?3,
           delivered_at = CASE WHEN ?2 = 'delivered' THEN ?4 ELSE delivered_at END
         WHERE id = ?1",
        params![id, state, error, now],
    )
    .map_err(sql)?;
    Ok(())
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
        self.query(move |conn| {
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
        .await
    }

    pub async fn request_update(&self, input: RequestUpdateInput) -> R<RequestUpdateOutput> {
        let (changed, request) = self
            .transact(false, move |conn, now| update(conn, now, &input))
            .await?;
        if changed {
            self.hub.requests();
        }
        Ok(RequestUpdateOutput { changed, request })
    }

    /// Tells the delivery service a request was just stored.
    pub(crate) fn request_added(&self) {
        self.hub.requests();
        self.requests.wake.notify_one();
    }

    /// Sends the notice of every request that is due (a few per pass) and
    /// returns how many were attempted. Each attempt is reserved before the
    /// request goes out, so a crash mid-request waits out the backoff; the
    /// receiver collapses a resend by the `Idempotency-Key`, which is the
    /// request's id on every attempt.
    pub async fn deliver_requests(&self) -> R<usize> {
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
        let mut attempted = 0;
        for row in due {
            let Some(target) = self.requests.target.clone() else {
                let id = row.id.clone();
                self.transact(false, move |conn, now| {
                    settle(conn, &id, delivery::UNCONFIGURED, None, now)
                })
                .await?;
                tracing::info!(
                    target: LOG,
                    request = %row.id,
                    "Recipe request notice not sent: RECIPES_REQUEST_WEBHOOK_URL is not set"
                );
                continue;
            };
            // An agent that already took the request needs no notice.
            if row.status != "queued" {
                let id = row.id.clone();
                self.transact(false, move |conn, now| {
                    settle(conn, &id, delivery::SKIPPED, None, now)
                })
                .await?;
                continue;
            }
            let app_url = self.app_url.clone();
            let reserve = row.clone();
            let attempts = row.attempts + 1;
            let payload = self
                .transact(false, move |conn, now| {
                    conn.execute(
                        "UPDATE recipes_request SET attempts = ?2, next_attempt_at = ?3 WHERE id = ?1",
                        params![reserve.id, attempts, now + backoff_ms(attempts)],
                    )
                    .map_err(sql)?;
                    body(conn, &reserve, app_url.as_deref())
                })
                .await?;
            attempted += 1;
            let result = self
                .requests
                .transport
                .post(&target, &row.id, &omni_core::js::json_stringify(&payload))
                .await;
            let host = target.url.host_str().unwrap_or("").to_owned();
            let outcome = classify(&result);
            let now = self.now();
            let (state, error) = match &outcome {
                Attempt::Delivered => {
                    tracing::info!(
                        target: LOG,
                        request = %row.id,
                        host = %host,
                        attempts,
                        "Recipe request notice delivered"
                    );
                    (delivery::DELIVERED, None)
                }
                Attempt::Ignored => {
                    tracing::warn!(
                        target: LOG,
                        request = %row.id,
                        host = %host,
                        "Recipe request notice accepted, but the receiving bot has no active webhook routine; the request stays queued"
                    );
                    (delivery::IGNORED, None)
                }
                Attempt::Retry(error) if now - row.created_at < GIVE_UP_AFTER_MS => {
                    tracing::info!(
                        target: LOG,
                        request = %row.id,
                        host = %host,
                        attempts,
                        error = %error,
                        "Recipe request notice failed; will retry"
                    );
                    (delivery::PENDING, Some(error.clone()))
                }
                Attempt::Retry(error) | Attempt::Rejected(error) => {
                    tracing::warn!(
                        target: LOG,
                        request = %row.id,
                        host = %host,
                        attempts,
                        error = %error,
                        "Recipe request notice failed; giving up. The request stays queued for recipes_requests_list"
                    );
                    (delivery::FAILED, Some(error.clone()))
                }
            };
            let id = row.id.clone();
            self.transact(false, move |conn, now| {
                settle(conn, &id, state, error.as_deref(), now)
            })
            .await?;
        }
        Ok(attempted)
    }

    /// Requests written while no webhook was configured, and still waiting
    /// for an agent, are owed a notice once one is: the service queues them
    /// when it starts. Returns how many.
    pub async fn announce_unannounced(&self) -> R<usize> {
        if self.requests.target.is_none() {
            return Ok(0);
        }
        self.transact(false, |conn, now| {
            conn.execute(
                "UPDATE recipes_request SET delivery = 'pending', next_attempt_at = ?1
                 WHERE delivery = 'unconfigured' AND status = 'queued'",
                params![now],
            )
            .map_err(sql)
        })
        .await
    }

    /// The delivery service: sends due notices when a request is stored and
    /// every few seconds, until shutdown. A failed pass is logged and the
    /// next one runs as usual.
    pub async fn run_requests(&self, shutdown: tokio_util::sync::CancellationToken) {
        match &self.requests.target {
            Some(target) => tracing::info!(
                target: LOG,
                host = target.url.host_str().unwrap_or(""),
                "Recipe requests are announced to the configured webhook"
            ),
            None => tracing::info!(
                target: LOG,
                "Recipe request webhook is disabled (RECIPES_REQUEST_WEBHOOK_URL is not set); requests stay in the library for recipes_requests_list"
            ),
        }
        if let Err(error) = self.announce_unannounced().await {
            tracing::warn!(target: LOG, "Recipe requests could not be queued for the webhook: {}", error.tool_text());
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
                tracing::warn!(target: LOG, "Recipe request delivery pass failed: {}", error.tool_text());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(status: u16, ignored: bool) -> Result<WebhookResponse, WebhookError> {
        Ok(WebhookResponse { status, ignored })
    }

    #[test]
    fn an_answer_is_delivered_retried_or_refused() {
        assert_eq!(classify(&answer(200, false)), Attempt::Delivered);
        assert_eq!(classify(&answer(204, false)), Attempt::Delivered);
        assert_eq!(classify(&answer(200, true)), Attempt::Ignored);
        for status in [408, 425, 429, 500, 502, 503] {
            assert!(matches!(
                classify(&answer(status, false)),
                Attempt::Retry(_)
            ));
        }
        for status in [301, 400, 401, 404, 409, 413] {
            assert!(matches!(
                classify(&answer(status, false)),
                Attempt::Rejected(_)
            ));
        }
        assert!(matches!(
            classify(&Err(WebhookError::Timeout)),
            Attempt::Retry(_)
        ));
        assert!(matches!(
            classify(&Err(WebhookError::Unusable)),
            Attempt::Rejected(_)
        ));
    }

    #[test]
    fn the_wait_doubles_to_thirty_minutes() {
        assert_eq!(backoff_ms(1), 30_000);
        assert_eq!(backoff_ms(2), 60_000);
        assert_eq!(backoff_ms(4), 240_000);
        assert_eq!(backoff_ms(7), MAX_BACKOFF_MS);
        assert_eq!(backoff_ms(400), MAX_BACKOFF_MS);
    }
}
