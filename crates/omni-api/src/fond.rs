//! Fond, the recipe app served on its own host: the contract between
//! `omni-recipes` and the `omni-recipes-web` PWA (docs/recipes.md).
//!
//! A recipe document travels as JSON (`doc`); both sides decode it with
//! `omni-recipe-format`, which this crate cannot depend on.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::common::Ms;

pub mod paths {
    use crate::common::encode_uri_component;

    /// `GET /api/sync?since=<seq>`.
    pub const SYNC: &str = "/api/sync";
    /// `GET /api/events` (SSE; frames named by [`super::sse`]).
    pub const EVENTS: &str = "/api/events";
    /// `POST /api/ops`.
    pub const OPS: &str = "/api/ops";
    /// `POST /api/cook` starts a session.
    pub const COOK: &str = "/api/cook";
    /// `GET /api/inbox` lists; `POST /api/inbox` uploads one photo.
    pub const INBOX: &str = "/api/inbox";
    /// The header every non-JSON upload must carry (any value).
    pub const UPLOAD_HEADER: &str = "x-fond-upload";

    pub fn sync(since: Option<u64>) -> String {
        match since {
            Some(seq) => format!("{SYNC}?since={seq}"),
            None => SYNC.to_owned(),
        }
    }

    /// `GET /api/cook/:id`.
    pub fn cook(session_id: &str) -> String {
        format!("{COOK}/{}", encode_uri_component(session_id))
    }

    /// `POST /api/cook/:id/events`.
    pub fn cook_events(session_id: &str) -> String {
        format!("{}/events", cook(session_id))
    }

    /// `POST /api/cook/:id/photo?id=` (image body, [`UPLOAD_HEADER`]). The
    /// client-chosen `upload_id` makes a retry store the photo once.
    pub fn cook_photo(session_id: &str, upload_id: &str) -> String {
        format!(
            "{}/photo?id={}",
            cook(session_id),
            encode_uri_component(upload_id)
        )
    }

    /// `GET /api/original/:recipeId`.
    pub fn original(recipe_id: &str) -> String {
        format!("/api/original/{}", encode_uri_component(recipe_id))
    }

    /// `POST /api/inbox?item=&index=&count=&title=`: photo `index` of `count`
    /// for the client-named inbox item.
    pub fn inbox_upload(item: &str, index: u32, count: u32, title: Option<&str>) -> String {
        let mut path = format!(
            "{INBOX}?item={}&index={index}&count={count}",
            encode_uri_component(item)
        );
        if let Some(title) = title {
            path.push_str("&title=");
            path.push_str(&encode_uri_component(title));
        }
        path
    }

    /// `GET /img/:digest/:variant.jpg`, immutable.
    pub fn image(digest: &str, variant: super::ImageVariant) -> String {
        format!("/img/{digest}/{}.jpg", variant.as_str())
    }

    /// `GET /src/:itemId/:n.jpg`: photo `n` of an inbox item.
    pub fn source_photo(item_id: &str, index: u32) -> String {
        format!("/src/{}/{index}.jpg", encode_uri_component(item_id))
    }
}

/// SSE frame names on `/api/events`.
pub mod sse {
    /// First frame on every connection: [`super::Hello`].
    pub const HELLO: &str = "hello";
    /// The library changed: [`super::LibraryChanged`]. Sync again.
    pub const LIBRARY: &str = "library";
    /// A cook session changed: [`super::SessionChanged`]. Fetch it again.
    pub const SESSION: &str = "session";
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hello {
    pub seq: u64,
    /// The server build; a change means a new app shell is available.
    pub build: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryChanged {
    pub seq: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionChanged {
    pub id: String,
    /// The session's newest event sequence number.
    pub seq: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ImageVariant {
    /// 480 px on the long edge.
    #[serde(rename = "thumb")]
    Thumb,
    /// 1600 px on the long edge.
    #[serde(rename = "full")]
    Full,
}

impl ImageVariant {
    pub fn as_str(self) -> &'static str {
        match self {
            ImageVariant::Thumb => "thumb",
            ImageVariant::Full => "full",
        }
    }
}

/// `GET /api/sync`. With `since`, `recipes` holds every recipe changed after
/// that sequence number and `removed` every id that left the library (trashed
/// or merged away) after it. Without `since`, or when the server cannot answer
/// incrementally, `full` is true and `recipes` is the whole library. A client
/// whose recipe count differs from `live_count` after applying asks again
/// without `since`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncResponse {
    pub seq: u64,
    pub full: bool,
    pub build: Option<String>,
    /// Active and archived recipes in the library.
    pub live_count: u32,
    pub recipes: Vec<RecipeRecord>,
    pub removed: Vec<String>,
    /// The whole tag registry (small).
    pub tags: Vec<Tag>,
    /// Every open cook session, with its events.
    pub sessions: Vec<CookSession>,
    /// Inbox items waiting for an agent.
    pub inbox_pending: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RecipeState {
    #[serde(rename = "active")]
    Active,
    #[serde(rename = "archived")]
    Archived,
    #[serde(rename = "trashed")]
    Trashed,
}

/// One recipe as the app stores it: the head document plus record metadata.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecipeRecord {
    pub id: String,
    pub revision: u32,
    /// `omni_recipe_format::FORMAT` of `doc`; the server upgrades on read.
    pub format: u32,
    pub state: RecipeState,
    /// The head `RecipeDoc`.
    pub doc: Value,
    pub tags: Vec<String>,
    pub notes: String,
    pub notes_rev: u32,
    /// In display order.
    pub images: Vec<ImageInfo>,
    /// The cover's image id; the first image when unset.
    pub cover: Option<String>,
    pub source: Option<SourceRef>,
    pub active_min: u32,
    pub total_min: u32,
    pub cook_count: u32,
    pub last_cooked_at: Option<Ms>,
    pub created_at: Ms,
    pub updated_at: Ms,
    pub seq: u64,
    /// Open adjustments from earlier cooks, oldest first.
    pub adjustments: Vec<Adjustment>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageInfo {
    pub id: String,
    /// SHA-256 of the original bytes; names the `/img` URL.
    pub digest: String,
    pub width: u32,
    pub height: u32,
    /// Average colour, `#rrggbb`, painted before the image loads.
    pub color: String,
    /// `dish`, `step` or `source`.
    pub role: String,
    pub caption: Option<String>,
}

/// Where a recipe came from, for the Original section.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceRef {
    pub item_id: String,
    /// `url`, `text` or `photo`.
    pub kind: String,
    pub url: Option<String>,
    /// Photos stored with the item (`/src/:itemId/:n.jpg`).
    pub files: u32,
}

/// `GET /api/original/:recipeId`: the stored source snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Original {
    pub item_id: String,
    pub kind: String,
    pub title: Option<String>,
    pub url: Option<String>,
    pub added_at: Ms,
    /// The page or pasted text as stored.
    pub text: Option<String>,
    pub files: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tag {
    pub name: String,
    /// `course`, `cuisine`, `main`, `method`, `diet`, `occasion` or `other`.
    pub group: String,
    pub description: Option<String>,
    /// Active recipes carrying the tag.
    pub count: u32,
}

/// What one cook changed, kept on the recipe until an agent folds it in or it
/// is dismissed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Adjustment {
    pub id: String,
    /// A step or ingredient id.
    pub target: String,
    /// The rendered text the cook saw before changing it.
    pub original: String,
    pub text: String,
    pub session_id: String,
    pub cooked_at: Ms,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CookState {
    #[serde(rename = "active")]
    Active,
    /// Finished by the cook, or closed after 18 idle hours with at least half
    /// its steps done.
    #[serde(rename = "finished")]
    Finished,
    /// Closed after 18 idle hours with fewer than half its steps done.
    #[serde(rename = "abandoned")]
    Abandoned,
}

/// A cook session: one pinned revision and scale, and an append-only event log.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CookSession {
    pub id: String,
    pub recipe_id: String,
    pub revision: u32,
    pub scale: f64,
    pub state: CookState,
    pub started_at: Ms,
    pub finished_at: Option<Ms>,
    pub last_event_at: Ms,
    /// In sequence order.
    pub events: Vec<CookEvent>,
    /// The pinned revision's `RecipeDoc`, so a session renders the recipe it
    /// started on from any device. Absent only on a request body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doc: Option<Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CookActor {
    #[serde(rename = "app")]
    App,
    #[serde(rename = "agent")]
    Agent,
    /// The server itself (idle close).
    #[serde(rename = "system")]
    System,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CookEvent {
    /// Assigned by the server; 0 on an event the client has not sent yet.
    #[serde(default)]
    pub seq: u32,
    /// Chosen by the sender; the server applies each id once.
    pub client_event_id: String,
    pub at: Ms,
    pub actor: CookActor,
    #[serde(flatten)]
    pub kind: CookEventKind,
}

/// `target` of `check`/`uncheck` is a step id (the step is done), an
/// ingredient id (gathered), or `step/ref` (`step/ref:part`) for one input of
/// a step, the same keys as the chart's leaf cells.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum CookEventKind {
    Check {
        target: String,
    },
    Uncheck {
        target: String,
    },
    /// Replaces the rendered text of a step or ingredient for this session.
    Adjust {
        target: String,
        text: String,
    },
    /// Removes this session's adjustment of `target`.
    Unadjust {
        target: String,
    },
    Note {
        text: String,
    },
    /// Starts (or restarts) the timer of `step`.
    #[serde(rename_all = "camelCase")]
    Timer {
        step: String,
        ends_at: Ms,
    },
    CancelTimer {
        step: String,
    },
    /// Moves the session to a newer revision of its recipe; state carries
    /// over by id.
    Rebase {
        revision: u32,
    },
    Finish {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    /// Discards the session: it closes as abandoned at once, counts as no
    /// cook, leaves no adjustments and drops its timers.
    Abandon {},
    /// Closes an open adjustment record; valid on a finished session too.
    DismissAdjustment {
        id: String,
    },
}

/// `POST /api/cook`. Repeating a request with the same `id` returns the
/// existing session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartCookRequest {
    /// `cook_` plus 8 lowercase letters or digits, chosen by the client.
    pub id: String,
    pub recipe_id: String,
    pub revision: u32,
    pub scale: f64,
    pub at: Ms,
}

/// `POST /api/cook/:id/events`; answers with the [`CookSession`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppendCookEventsRequest {
    pub events: Vec<CookEvent>,
}

/// `POST /api/ops`: the app's record edits, each applied once by `id`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpsRequest {
    pub ops: Vec<ClientOp>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientOp {
    /// Chosen by the client (`op_` plus 12 lowercase letters or digits).
    pub id: String,
    pub at: Ms,
    #[serde(flatten)]
    pub kind: ClientOpKind,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ClientOpKind {
    /// Sets the notes field. When `base_rev` is stale both texts are kept,
    /// the server's first.
    #[serde(rename_all = "camelCase")]
    Notes {
        recipe_id: String,
        text: String,
        base_rev: u32,
    },
    /// Archive, delete (trash) or bring back.
    #[serde(rename_all = "camelCase")]
    State {
        recipe_id: String,
        state: RecipeState,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpsResponse {
    pub seq: u64,
    /// One per op, in request order.
    pub results: Vec<OpResult>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpResult {
    pub id: String,
    /// False when the op can never apply (unknown recipe); the client drops it.
    pub ok: bool,
    pub error: Option<String>,
    /// The recipe after the op, unless it is now trashed.
    pub recipe: Option<RecipeRecord>,
}

/// `GET /api/inbox`: the newest items, pending first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InboxList {
    pub items: Vec<InboxItem>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InboxItem {
    pub id: String,
    /// `photo`, `url` or `text`.
    pub kind: String,
    pub title: Option<String>,
    pub added_at: Ms,
    pub files: u32,
    /// `pending`, `imported`, `discarded` or `duplicate`.
    pub state: String,
    pub recipe_id: Option<String>,
}

/// Answer to one uploaded photo.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InboxUploadResponse {
    pub item_id: String,
    pub received: u32,
    /// True once every photo of the item has arrived; agents see it from then.
    pub complete: bool,
}
