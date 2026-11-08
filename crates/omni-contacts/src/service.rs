//! The contacts service: reads over the directory index, and idempotent,
//! verified writes.
//!
//! A write is planned against cards read fresh by GET (strict UTF-8, with
//! their ETag), reserved durably under its idempotency key, then sent one
//! conditional request at a time (If-Match, or If-None-Match for creates).
//! Each request is verified by reading the card back and comparing every
//! line digest with the plan: only a read-back match counts as done, an
//! applied write whose card lost or changed unedited lines is `diverged`,
//! and an uncertain request is settled only by reading, never resent.
//! Before a card is overwritten or deleted its previous text is kept as a
//! private recovery copy until the write verifies.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use omni_core::clock::SharedClock;
use omni_core::digest::sha256_hex;
use omni_http::{HttpClient, SideEffectMode, Url};
use omni_store::Store;
use omni_store::entity::{EntityOps as _, EntityWrite as _, UpsertOpts};
use tokio::sync::Mutex;
use tokio_util::task::TaskTracker;

use crate::dav::{Dav, Precondition, WriteError};
use crate::directory::{Book, BookInfo, Directory, Snapshot, valid_segment};
use crate::edit::{self, ContactPatch, Expectation};
use crate::error::ContactsError;
use crate::mcp::defs::{
    AddressOut, BookOut, ContactDetail, ContactSummary, GroupOut, GroupRef, GroupsListOutput,
    NameOut, PlannedWrite, PreviewOutput, PropertyCount, SearchOutput, StatusOutput, StepOut,
    ValueField, ValueOut, WriteErrorOut, WriteOutput, WriteStatusOutput,
};
use crate::model::{self, Kind, Projection};
use crate::store::{
    OperationErrorRecord, OperationRecord, OperationState, OperationStep, RECOVERY_TTL_MS,
    RecoveryCopy, StepKind, StepState, recovery_key,
};
use crate::vcard::Card;

const LOG: &str = "Contacts";

/// An uncertain request whose card still shows its old version this long
/// after the reservation is settled as not applied.
const SETTLE_AFTER_MS: i64 = 10 * 60 * 1000;

/// What this service can and cannot see, reported by `contacts_status`.
pub const LIMITS: &[&str] = &[
    "Covers every address book of the configured iCloud account over CardDAV. Contacts in other accounts (Google, Exchange, CardDAV servers) or stored only on a device are not reachable.",
    "Edits rewrite only the vCard lines they name and verify every other line by reading the card back. Data iOS may keep outside the vCard, such as Contact Poster assets, is not visible to CardDAV, so its preservation is not verified. A PHOTO kept as a URL is preserved as that URL, not as image bytes.",
    "Group membership changes rewrite only group cards; member contact cards are never written by them.",
];

pub fn is_valid_key(key: &str) -> bool {
    (16..=128).contains(&key.len())
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Login for iCloud CardDAV (the account's app-specific password).
#[derive(Clone)]
pub struct ContactsSettings {
    pub username: String,
    pub password: String,
}

impl std::fmt::Debug for ContactsSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContactsSettings")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

impl ContactsSettings {
    /// `Some` when `ICLOUD_USERNAME` and `ICLOUD_APP_PASSWORD` are set (the
    /// same login as iCloud mail and calendar).
    pub fn from_config(config: &omni_config::Config) -> Option<Self> {
        let username = config.icloud_username.clone().filter(|v| !v.is_empty())?;
        let password = config
            .icloud_app_password
            .clone()
            .filter(|v| !v.is_empty())?;
        Some(Self { username, password })
    }
}

pub struct ContactsDeps {
    pub http: HttpClient,
    pub settings: Option<ContactsSettings>,
    pub store: Store,
    pub clock: SharedClock,
    pub mode: SideEffectMode,
    pub tracker: Option<TaskTracker>,
}

/// A requested write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    GroupCreate {
        book_id: Option<String>,
        name: String,
        contact_ids: Vec<String>,
    },
    GroupRename {
        group_id: String,
        etag: Option<String>,
        name: String,
    },
    GroupDelete {
        group_id: String,
        etag: String,
    },
    Members {
        add: Vec<(String, Vec<String>)>,
        remove: Vec<(String, Vec<String>)>,
        moves: Vec<(String, String, Vec<String>)>,
    },
    ContactCreate {
        book_id: Option<String>,
        patch: Box<ContactPatch>,
        group_ids: Vec<String>,
    },
    ContactsUpdate {
        changes: Vec<(String, String, ContactPatch)>,
    },
}

/// A contacts search.
#[derive(Clone, Debug, Default)]
pub struct SearchQuery<'a> {
    pub query: Option<&'a str>,
    pub book_id: Option<&'a str>,
    pub group_id: Option<&'a str>,
    pub ungrouped: bool,
    pub cursor: usize,
    pub limit: usize,
    pub fresh: bool,
}

/// Member UIDs a group gains and loses.
type MemberChange = (Vec<String>, Vec<String>);

#[derive(Clone, Debug)]
pub struct WriteRequest {
    pub tool: &'static str,
    pub idempotency_key: String,
    /// SHA-256 of the canonical tool input.
    pub fingerprint: String,
    pub action: Action,
}

/// A card read fresh: its ETag, exact text and parse.
struct Fetched {
    etag: String,
    text: String,
    card: Card,
}

struct PlannedStep {
    kind: StepKind,
    resource_id: String,
    url: Url,
    purpose: &'static str,
    precondition: Precondition,
    body: Option<String>,
    before: Option<Fetched>,
    after: Option<Card>,
    expect: Option<Expectation>,
    changes: Vec<String>,
}

struct Plan {
    steps: Vec<PlannedStep>,
    dependent: bool,
    status: &'static str,
    contact_id: Option<String>,
    group_id: Option<String>,
    unchanged: Vec<String>,
    warnings: Vec<String>,
}

impl Plan {
    fn new(status: &'static str, dependent: bool) -> Self {
        Self {
            steps: Vec::new(),
            dependent,
            status,
            contact_id: None,
            group_id: None,
            unchanged: Vec::new(),
            warnings: Vec::new(),
        }
    }
}

/// What the operation row keeps besides its steps: ids and states only.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Summary {
    status: String,
    contact_id: Option<String>,
    group_id: Option<String>,
    unchanged: Vec<String>,
    warnings: Vec<String>,
}

#[derive(Clone)]
pub struct Contacts {
    inner: Arc<Inner>,
}

struct Inner {
    directory: Option<Directory>,
    store: Store,
    clock: SharedClock,
    tracker: Option<TaskTracker>,
    write_lock: Arc<Mutex<()>>,
}

fn ms_text(ms: i64) -> String {
    jiff::Timestamp::from_millisecond(ms)
        .map(|t| t.to_string())
        .unwrap_or_else(|_| ms.to_string())
}

pub fn split_id(id: &str) -> Result<(&str, &str), ContactsError> {
    match id.split_once('/') {
        Some((book, resource)) if valid_segment(book) && valid_segment(resource) => {
            Ok((book, resource))
        }
        _ => Err(ContactsError::invalid(format!(
            "{id:?} is not a contact or group id (bookId/resource)"
        ))),
    }
}

pub fn resource_id(book: &str, resource: &str) -> String {
    format!("{book}/{resource}")
}

fn value_field(field: crate::labels::Field) -> ValueField {
    use crate::labels::Field;
    match field {
        Field::Email => ValueField::Email,
        Field::Phone => ValueField::Phone,
        Field::Address => ValueField::Address,
        Field::Url => ValueField::Url,
        Field::InstantMessage => ValueField::InstantMessage,
        Field::SocialProfile => ValueField::SocialProfile,
        Field::RelatedName => ValueField::RelatedName,
        Field::Date => ValueField::Date,
    }
}

/// Group ids (in this book) that list `uid`.
fn groups_of(book: &Book, uid: Option<&str>) -> Vec<(String, String)> {
    let Some(uid) = uid else {
        return Vec::new();
    };
    book.groups()
        .filter(|(_, g)| g.members.iter().any(|m| m.eq_ignore_ascii_case(uid)))
        .map(|(e, g)| (resource_id(&book.info.id, &e.resource), g.display_name()))
        .collect()
}

/// Shortens a line for a preview (photos are long).
fn preview_line(text: &str) -> String {
    if text.len() <= 200 {
        return text.to_owned();
    }
    let head: String = text.chars().take(120).collect();
    format!("{head}… ({} bytes)", text.len())
}

/// Unfolded lines removed and added between two versions of a card.
fn line_diff(before: Option<&Card>, after: Option<&Card>) -> (Vec<String>, Vec<String>) {
    let count = |card: Option<&Card>| {
        let mut counts: BTreeMap<String, i64> = BTreeMap::new();
        for line in card.map(Card::lines).unwrap_or_default() {
            *counts.entry(line.text().to_owned()).or_default() += 1;
        }
        counts
    };
    let old = count(before);
    let new = count(after);
    let mut removed = Vec::new();
    let mut added = Vec::new();
    for (text, n) in &old {
        for _ in 0..(n - new.get(text).copied().unwrap_or(0)).max(0) {
            removed.push(preview_line(text));
        }
    }
    for (text, n) in &new {
        for _ in 0..(n - old.get(text).copied().unwrap_or(0)).max(0) {
            added.push(preview_line(text));
        }
    }
    (removed, added)
}

enum Sent {
    Applied {
        status: u16,
        etag: Option<String>,
    },
    Rejected {
        status: Option<u16>,
        code: &'static str,
        message: String,
    },
    Uncertain {
        status: Option<u16>,
        message: String,
    },
}

fn classify(result: &Result<crate::dav::DavResponse, WriteError>) -> Sent {
    match result {
        Err(WriteError::NotSent(message)) => Sent::Rejected {
            status: None,
            code: "not_sent",
            message: format!("not sent: {message}"),
        },
        Err(WriteError::Uncertain(message)) => Sent::Uncertain {
            status: None,
            message: message.clone(),
        },
        Ok(response) => {
            let status = response.status;
            match status {
                200..=299 => Sent::Applied {
                    status,
                    etag: response.etag.clone(),
                },
                412 => Sent::Rejected {
                    status: Some(status),
                    code: "version_conflict",
                    message: "the card changed since it was read; read it again".to_owned(),
                },
                404 => Sent::Rejected {
                    status: Some(status),
                    code: "not_found",
                    message: "the card no longer exists".to_owned(),
                },
                401 | 403 => Sent::Rejected {
                    status: Some(status),
                    code: "unauthorized",
                    message: format!("iCloud refused the write (HTTP {status})"),
                },
                500..=599 => Sent::Uncertain {
                    status: Some(status),
                    message: format!("iCloud answered HTTP {status}; the write may have applied"),
                },
                _ => Sent::Rejected {
                    status: Some(status),
                    code: "rejected",
                    message: format!("iCloud refused the write (HTTP {status})"),
                },
            }
        }
    }
}

impl Contacts {
    pub fn new(deps: ContactsDeps) -> Self {
        let directory = deps.settings.as_ref().map(|s| {
            Directory::new(
                Dav::new(deps.http.clone(), &s.username, &s.password, deps.mode),
                deps.clock.clone(),
            )
        });
        Self {
            inner: Arc::new(Inner {
                directory,
                store: deps.store,
                clock: deps.clock,
                tracker: deps.tracker,
                write_lock: Arc::new(Mutex::new(())),
            }),
        }
    }

    pub fn configured(&self) -> bool {
        self.inner.directory.is_some()
    }

    fn directory(&self) -> Result<&Directory, ContactsError> {
        self.inner
            .directory
            .as_ref()
            .ok_or(ContactsError::NotConfigured)
    }

    fn now_ms(&self) -> i64 {
        self.inner.clock.now_ms()
    }

    /// Writes captured in record mode (tests and shadow runs).
    pub fn recorded_writes(&self) -> Vec<crate::dav::RecordedWrite> {
        self.inner
            .directory
            .as_ref()
            .map(|d| d.dav().recorded())
            .unwrap_or_default()
    }

    // ---- reads ----

    pub async fn status(&self) -> StatusOutput {
        let limits = LIMITS.iter().map(|s| (*s).to_owned()).collect();
        let Ok(directory) = self.directory() else {
            return StatusOutput {
                configured: false,
                state: "not_configured".to_owned(),
                error_code: Some("not_configured".to_owned()),
                message: Some(ContactsError::NotConfigured.to_string()),
                books: Vec::new(),
                synced_at: None,
                limits,
            };
        };
        match directory.snapshot(false).await {
            Ok(snapshot) => StatusOutput {
                configured: true,
                state: if snapshot.stale { "stale" } else { "ready" }.to_owned(),
                error_code: None,
                message: snapshot.error.clone(),
                books: snapshot
                    .books
                    .iter()
                    .map(|book| {
                        let count = |kind: Kind| {
                            book.entries
                                .values()
                                .filter(|e| e.view.as_ref().is_some_and(|v| v.kind == kind))
                                .count() as u64
                        };
                        BookOut {
                            book_id: book.info.id.clone(),
                            name: book.info.name.clone(),
                            contact_count: count(Kind::Contact),
                            group_count: count(Kind::Group),
                            unreadable_count: book
                                .entries
                                .values()
                                .filter(|e| e.view.is_none())
                                .count() as u64
                                + u64::from(book.skipped),
                        }
                    })
                    .collect(),
                synced_at: snapshot.synced_at.map(ms_text),
                limits,
            },
            Err(error) => StatusOutput {
                configured: true,
                state: "error".to_owned(),
                error_code: Some(error.code().to_owned()),
                message: Some(error.to_string()),
                books: Vec::new(),
                synced_at: None,
                limits,
            },
        }
    }

    async fn snapshot(&self, fresh: bool) -> Result<Snapshot, ContactsError> {
        self.directory()?.snapshot(fresh).await
    }

    /// A fresh index for planning a write; a refresh that failed (and so
    /// served the previous index) is refused rather than planned against.
    async fn write_snapshot(&self) -> Result<Snapshot, ContactsError> {
        let snapshot = self.snapshot(true).await?;
        if snapshot.stale {
            return Err(ContactsError::coded(
                "stale_index",
                format!(
                    "iCloud contacts could not be refreshed, so the write was not planned: {}",
                    snapshot.error.clone().unwrap_or_default()
                ),
            ));
        }
        Ok(snapshot)
    }

    fn unreadable(snapshot: &Snapshot, book_id: Option<&str>) -> u64 {
        snapshot
            .books
            .iter()
            .filter(|b| book_id.is_none_or(|id| b.info.id == id))
            .map(|b| {
                b.entries.values().filter(|e| e.view.is_none()).count() as u64
                    + u64::from(b.skipped)
            })
            .sum()
    }

    pub async fn search(&self, search: SearchQuery<'_>) -> Result<SearchOutput, ContactsError> {
        let SearchQuery {
            query,
            book_id,
            group_id,
            ungrouped,
            cursor,
            limit,
            fresh,
        } = search;
        let snapshot = self.snapshot(fresh).await?;
        if let Some(book_id) = book_id
            && snapshot.book(book_id).is_none()
        {
            return Err(ContactsError::coded(
                "not_found",
                format!("no address book {book_id}"),
            ));
        }
        let group_filter = match group_id {
            Some(id) => {
                let (book, resource) = split_id(id)?;
                let members = snapshot
                    .book(book)
                    .and_then(|b| b.entries.get(resource))
                    .and_then(|e| e.view.clone())
                    .filter(|v| v.kind == Kind::Group)
                    .ok_or_else(|| ContactsError::coded("not_found", format!("no group {id}")))?;
                let set: BTreeSet<String> = members
                    .members
                    .iter()
                    .map(|m| m.to_ascii_lowercase())
                    .collect();
                Some((book.to_owned(), set))
            }
            None => None,
        };
        let mut hits: Vec<(String, ContactSummary)> = Vec::new();
        for book in snapshot.books.iter() {
            if book_id.is_some_and(|id| id != book.info.id) {
                continue;
            }
            if let Some((group_book, _)) = &group_filter
                && *group_book != book.info.id
            {
                continue;
            }
            let mut memberships: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for (entry, group) in book.groups() {
                for uid in &group.members {
                    memberships
                        .entry(uid.to_ascii_lowercase())
                        .or_default()
                        .push(resource_id(&book.info.id, &entry.resource));
                }
            }
            for entry in book.entries.values() {
                let Some(view) = entry.view.as_ref().filter(|v| v.kind == Kind::Contact) else {
                    continue;
                };
                let uid = view.uid.as_deref().map(str::to_ascii_lowercase);
                let groups = uid
                    .as_deref()
                    .and_then(|u| memberships.get(u))
                    .cloned()
                    .unwrap_or_default();
                if let Some((_, members)) = &group_filter
                    && !uid.as_deref().is_some_and(|u| members.contains(u))
                {
                    continue;
                }
                if ungrouped && !groups.is_empty() {
                    continue;
                }
                if !query.is_none_or(|q| view.matches(q)) {
                    continue;
                }
                let name = view.display_name();
                hits.push((
                    name.to_lowercase(),
                    ContactSummary {
                        contact_id: resource_id(&book.info.id, &entry.resource),
                        book_id: book.info.id.clone(),
                        etag: entry.etag.clone(),
                        display_name: name,
                        organization: view.organization.clone(),
                        emails: view
                            .values(crate::labels::Field::Email)
                            .map(|i| i.value.clone())
                            .collect(),
                        phones: view
                            .values(crate::labels::Field::Phone)
                            .map(|i| i.value.clone())
                            .collect(),
                        group_ids: groups,
                        has_photo: view.has_photo,
                    },
                ));
            }
        }
        hits.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then_with(|| a.1.contact_id.cmp(&b.1.contact_id))
        });
        let total = hits.len();
        let page: Vec<ContactSummary> = hits
            .into_iter()
            .skip(cursor)
            .take(limit)
            .map(|(_, s)| s)
            .collect();
        let next = cursor + page.len();
        Ok(SearchOutput {
            contacts: page,
            total: total as u64,
            next_cursor: (next < total).then(|| u32::try_from(next).unwrap_or(u32::MAX)),
            unreadable_count: Self::unreadable(&snapshot, book_id),
            synced_at: snapshot.synced_at.map(ms_text),
            stale: snapshot.stale,
        })
    }

    pub async fn groups(
        &self,
        book_id: Option<&str>,
        fresh: bool,
    ) -> Result<GroupsListOutput, ContactsError> {
        let snapshot = self.snapshot(fresh).await?;
        let mut groups = Vec::new();
        for book in snapshot.books.iter() {
            if book_id.is_some_and(|id| id != book.info.id) {
                continue;
            }
            let uids: BTreeSet<String> = book
                .entries
                .values()
                .filter_map(|e| e.view.as_ref())
                .filter(|v| v.kind == Kind::Contact)
                .filter_map(|v| v.uid.as_deref().map(str::to_ascii_lowercase))
                .collect();
            for (entry, group) in book.groups() {
                let unresolved = group
                    .members
                    .iter()
                    .filter(|m| !uids.contains(&m.to_ascii_lowercase()))
                    .count();
                groups.push(GroupOut {
                    group_id: resource_id(&book.info.id, &entry.resource),
                    book_id: book.info.id.clone(),
                    name: group.display_name(),
                    etag: entry.etag.clone(),
                    member_count: group.members.len() as u64,
                    unresolved_member_count: unresolved as u64,
                });
            }
        }
        groups.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.group_id.cmp(&b.group_id))
        });
        Ok(GroupsListOutput {
            groups,
            unreadable_count: Self::unreadable(&snapshot, book_id),
            synced_at: snapshot.synced_at.map(ms_text),
            stale: snapshot.stale,
        })
    }

    /// Reads one card fresh: strict UTF-8, its ETag, and a parse.
    async fn fetch(
        &self,
        book: &BookInfo,
        resource: &str,
    ) -> Result<Option<Fetched>, ContactsError> {
        let directory = self.directory()?;
        let url = book.resource_url(resource)?;
        let response = directory.dav().get(&url).await?;
        match response.status {
            404 => Ok(None),
            401 | 403 => {
                directory.forget().await;
                Err(ContactsError::status(response.status, "the card read"))
            }
            200..=299 => {
                let etag = response.etag.clone().ok_or_else(|| {
                    ContactsError::protocol("iCloud returned no ETag for the card")
                })?;
                let text = response
                    .utf8()
                    .map_err(|_| {
                        ContactsError::coded(
                            "undecodable_card",
                            "the card is not valid UTF-8 and cannot be edited without changing it",
                        )
                    })?
                    .to_owned();
                let card = Card::parse(&text)
                    .map_err(|e| ContactsError::coded("unreadable_card", e.to_string()))?;
                Ok(Some(Fetched { etag, text, card }))
            }
            status => Err(ContactsError::status(status, "the card read")),
        }
    }

    fn book_info(snapshot: &Snapshot, book_id: &str) -> Result<BookInfo, ContactsError> {
        snapshot
            .book(book_id)
            .map(|b| b.info.clone())
            .ok_or_else(|| ContactsError::coded("not_found", format!("no address book {book_id}")))
    }

    pub async fn contact(&self, contact_id: &str) -> Result<ContactDetail, ContactsError> {
        let (book_id, resource) = split_id(contact_id)?;
        let snapshot = self.snapshot(false).await?;
        let info = Self::book_info(&snapshot, book_id)?;
        let fetched = self
            .fetch(&info, resource)
            .await?
            .ok_or_else(|| ContactsError::coded("not_found", format!("no contact {contact_id}")))?;
        let view = model::project(&fetched.card);
        if view.kind == Kind::Group {
            return Err(ContactsError::invalid(format!(
                "{contact_id} is a group; use contacts_groups_list or contacts_search groupId"
            )));
        }
        let groups = snapshot
            .book(book_id)
            .map(|b| groups_of(b, view.uid.as_deref()))
            .unwrap_or_default()
            .into_iter()
            .map(|(group_id, name)| GroupRef { group_id, name })
            .collect();
        let mut blockers = Vec::new();
        if let Err(error) = edit::ensure_editable(&fetched.card) {
            blockers.push(error.to_string());
        }
        Ok(detail(
            contact_id,
            book_id,
            &fetched.etag,
            &view,
            groups,
            blockers,
        ))
    }

    // ---- writes ----

    fn resolve_book<'s>(
        snapshot: &'s Snapshot,
        book_id: Option<&str>,
    ) -> Result<&'s Book, ContactsError> {
        match book_id {
            Some(id) => snapshot
                .book(id)
                .ok_or_else(|| ContactsError::coded("not_found", format!("no address book {id}"))),
            None => match snapshot.books.as_slice() {
                [only] => Ok(only),
                _ => Err(ContactsError::coded(
                    "book_required",
                    "the account has several address books; pass bookId",
                )),
            },
        }
    }

    /// The UID of a contact in `book`, from the index.
    fn member_uid(book: &Book, contact_id: &str) -> Result<String, ContactsError> {
        let (book_id, resource) = split_id(contact_id)?;
        if book_id != book.info.id {
            return Err(ContactsError::coded(
                "cross_book",
                format!(
                    "{contact_id} is in another address book; groups hold members of their own book only"
                ),
            ));
        }
        let entry = book
            .entries
            .get(resource)
            .ok_or_else(|| ContactsError::coded("not_found", format!("no contact {contact_id}")))?;
        let view = entry.view.as_ref().ok_or_else(|| {
            ContactsError::coded("unreadable_card", format!("{contact_id} could not be read"))
        })?;
        if view.kind != Kind::Contact {
            return Err(ContactsError::invalid(format!(
                "{contact_id} is a group, not a contact"
            )));
        }
        view.uid
            .clone()
            .ok_or_else(|| ContactsError::coded("unsupported", format!("{contact_id} has no UID")))
    }

    fn group_entry<'s>(snapshot: &'s Snapshot, group_id: &str) -> Result<&'s Book, ContactsError> {
        let (book_id, resource) = split_id(group_id)?;
        let book = snapshot
            .book(book_id)
            .ok_or_else(|| ContactsError::coded("not_found", format!("no group {group_id}")))?;
        match book.entries.get(resource).and_then(|e| e.view.as_ref()) {
            Some(view) if view.kind == Kind::Group => Ok(book),
            Some(_) => Err(ContactsError::invalid(format!(
                "{group_id} is a contact, not a group"
            ))),
            None => Err(ContactsError::coded(
                "not_found",
                format!("no group {group_id}"),
            )),
        }
    }

    fn name_taken(book: &Book, name: &str, except: Option<&str>) -> bool {
        book.groups().any(|(entry, group)| {
            Some(entry.resource.as_str()) != except
                && group
                    .display_name()
                    .trim()
                    .eq_ignore_ascii_case(name.trim())
        })
    }

    fn put_step(
        resource_id: String,
        url: Url,
        purpose: &'static str,
        before: Option<Fetched>,
        edited: edit::Edited,
    ) -> PlannedStep {
        let precondition = match &before {
            Some(f) => Precondition::IfMatch(f.etag.clone()),
            None => Precondition::IfNoneMatchAny,
        };
        PlannedStep {
            kind: StepKind::Put,
            resource_id,
            url,
            purpose,
            precondition,
            body: Some(edited.card.to_text()),
            expect: Some(edit::expectation(
                before.as_ref().map(|f| &f.card),
                &edited.card,
            )),
            before,
            after: Some(edited.card),
            changes: edited.changes,
        }
    }

    async fn plan(&self, action: &Action, key: &str) -> Result<Plan, ContactsError> {
        let now = self.inner.clock.now();
        match action {
            Action::GroupCreate {
                book_id,
                name,
                contact_ids,
            } => {
                let snapshot = self.write_snapshot().await?;
                let book = Self::resolve_book(&snapshot, book_id.as_deref())?;
                edit::check_group_name(name)?;
                if Self::name_taken(book, name, None) {
                    return Err(ContactsError::coded(
                        "group_exists",
                        "the address book already has a group with this name",
                    ));
                }
                let uids = contact_ids
                    .iter()
                    .map(|id| Self::member_uid(book, id))
                    .collect::<Result<Vec<_>, _>>()?;
                let uid = edit::uid_from_seed(&format!("{key}:group"));
                let resource = format!("{uid}.vcf");
                if book.entries.contains_key(&resource) {
                    return Err(ContactsError::coded(
                        "already_exists",
                        "a group created with this idempotencyKey already exists",
                    ));
                }
                let card = edit::new_group(&uid, name, &uids, now)?;
                let id = resource_id(&book.info.id, &resource);
                let mut plan = Plan::new("created", true);
                plan.group_id = Some(id.clone());
                plan.steps.push(Self::put_step(
                    id,
                    book.info.resource_url(&resource)?,
                    "create group",
                    None,
                    edit::Edited {
                        changes: vec![format!("group created with {} member(s)", uids.len())],
                        card,
                    },
                ));
                Ok(plan)
            }
            Action::GroupRename {
                group_id,
                etag,
                name,
            } => {
                let snapshot = self.write_snapshot().await?;
                let book = Self::group_entry(&snapshot, group_id)?;
                let (_, resource) = split_id(group_id)?;
                if Self::name_taken(book, name, Some(resource)) {
                    return Err(ContactsError::coded(
                        "group_exists",
                        "another group in the address book has this name",
                    ));
                }
                let fetched = self
                    .fetch_current(&book.info, resource, group_id, etag.as_deref())
                    .await?;
                let edited = edit::rename_group(&fetched.card, name)?;
                let mut plan = Plan::new("updated", true);
                plan.group_id = Some(group_id.clone());
                if edited.unchanged() {
                    plan.status = "unchanged";
                    plan.unchanged.push(group_id.clone());
                } else {
                    plan.steps.push(Self::put_step(
                        group_id.clone(),
                        book.info.resource_url(resource)?,
                        "rename group",
                        Some(fetched),
                        edited,
                    ));
                }
                Ok(plan)
            }
            Action::GroupDelete { group_id, etag } => {
                let snapshot = self.snapshot(false).await?;
                let (book_id, resource) = split_id(group_id)?;
                let info = Self::book_info(&snapshot, book_id)?;
                let fetched = self
                    .fetch_current(&info, resource, group_id, Some(etag))
                    .await?;
                if model::kind_of(&fetched.card) != Kind::Group {
                    return Err(ContactsError::invalid(format!(
                        "{group_id} is a contact, not a group"
                    )));
                }
                let mut plan = Plan::new("deleted", true);
                plan.group_id = Some(group_id.clone());
                plan.steps.push(PlannedStep {
                    kind: StepKind::Delete,
                    resource_id: group_id.clone(),
                    url: info.resource_url(resource)?,
                    purpose: "delete group",
                    precondition: Precondition::IfMatch(fetched.etag.clone()),
                    body: None,
                    expect: None,
                    after: None,
                    changes: vec!["group deleted (member contacts are kept)".to_owned()],
                    before: Some(fetched),
                });
                Ok(plan)
            }
            Action::Members { add, remove, moves } => self.plan_members(add, remove, moves).await,
            Action::ContactCreate {
                book_id,
                patch,
                group_ids,
            } => {
                let snapshot = self.write_snapshot().await?;
                let book = Self::resolve_book(&snapshot, book_id.as_deref())?;
                let uid = edit::uid_from_seed(&format!("{key}:contact"));
                let resource = format!("{uid}.vcf");
                if book.entries.contains_key(&resource) {
                    return Err(ContactsError::coded(
                        "already_exists",
                        "a contact created with this idempotencyKey already exists",
                    ));
                }
                let edited = edit::new_contact(&uid, patch, now)?;
                let id = resource_id(&book.info.id, &resource);
                let mut plan = Plan::new("created", true);
                plan.contact_id = Some(id.clone());
                plan.steps.push(Self::put_step(
                    id,
                    book.info.resource_url(&resource)?,
                    "create contact",
                    None,
                    edited,
                ));
                let mut seen = BTreeSet::new();
                for group_id in group_ids {
                    if !seen.insert(group_id) {
                        continue;
                    }
                    let group_book = Self::group_entry(&snapshot, group_id)?;
                    if group_book.info.id != book.info.id {
                        return Err(ContactsError::coded(
                            "cross_book",
                            format!("{group_id} is in another address book"),
                        ));
                    }
                    let (_, group_resource) = split_id(group_id)?;
                    let fetched = self
                        .fetch_current(&book.info, group_resource, group_id, None)
                        .await?;
                    let edited =
                        edit::change_members(&fetched.card, std::slice::from_ref(&uid), &[])?;
                    plan.steps.push(Self::put_step(
                        group_id.clone(),
                        book.info.resource_url(group_resource)?,
                        "add members",
                        Some(fetched),
                        edited,
                    ));
                }
                Ok(plan)
            }
            Action::ContactsUpdate { changes } => {
                let snapshot = self.snapshot(false).await?;
                let mut seen = BTreeSet::new();
                let mut plan = Plan::new("updated", false);
                for (contact_id, etag, patch) in changes {
                    if !seen.insert(contact_id) {
                        return Err(ContactsError::invalid(format!(
                            "{contact_id} is listed more than once; combine its changes"
                        )));
                    }
                    let (book_id, resource) = split_id(contact_id)?;
                    let info = Self::book_info(&snapshot, book_id)?;
                    let fetched = self
                        .fetch_current(&info, resource, contact_id, Some(etag))
                        .await?;
                    if model::kind_of(&fetched.card) == Kind::Group {
                        return Err(ContactsError::invalid(format!(
                            "{contact_id} is a group; use the group tools"
                        )));
                    }
                    let edited = edit::apply_patch(&fetched.card, patch).map_err(|e| {
                        ContactsError::coded(e.code(), format!("{contact_id}: {e}"))
                    })?;
                    if edited.unchanged() {
                        plan.unchanged.push(contact_id.clone());
                        continue;
                    }
                    plan.steps.push(Self::put_step(
                        contact_id.clone(),
                        info.resource_url(resource)?,
                        "update contact",
                        Some(fetched),
                        edited,
                    ));
                }
                if plan.steps.is_empty() {
                    plan.status = "unchanged";
                }
                if changes.len() == 1 {
                    plan.contact_id = changes.first().map(|(id, _, _)| id.clone());
                }
                Ok(plan)
            }
        }
    }

    /// A card read fresh, refused when it is gone or its etag is not `etag`.
    async fn fetch_current(
        &self,
        book: &BookInfo,
        resource: &str,
        id: &str,
        etag: Option<&str>,
    ) -> Result<Fetched, ContactsError> {
        let fetched = self
            .fetch(book, resource)
            .await?
            .ok_or_else(|| ContactsError::coded("not_found", format!("{id} does not exist")))?;
        if !is_strong(&fetched.etag) {
            return Err(ContactsError::coded(
                "weak_etag",
                format!(
                    "iCloud gave {id} only a weak ETag, which cannot guard a conditional write; nothing was sent"
                ),
            ));
        }
        if let Some(expected) = etag
            && !same_etag(expected, &fetched.etag)
        {
            return Err(ContactsError::coded(
                "version_conflict",
                format!(
                    "{id} changed since it was read; its current etag is {}",
                    fetched.etag
                ),
            ));
        }
        Ok(fetched)
    }

    async fn plan_members(
        &self,
        add: &[(String, Vec<String>)],
        remove: &[(String, Vec<String>)],
        moves: &[(String, String, Vec<String>)],
    ) -> Result<Plan, ContactsError> {
        let snapshot = self.write_snapshot().await?;
        // Per group: (uids to add, uids to remove).
        let mut changes: BTreeMap<String, MemberChange> = BTreeMap::new();
        let mut request =
            |group_id: &str, contact_ids: &[String], adding: bool| -> Result<(), ContactsError> {
                let book = Self::group_entry(&snapshot, group_id)?;
                let entry = changes.entry(group_id.to_owned()).or_default();
                for contact_id in contact_ids {
                    let uid = Self::member_uid(book, contact_id)?;
                    let list = if adding { &mut entry.0 } else { &mut entry.1 };
                    if !list.iter().any(|u| u.eq_ignore_ascii_case(&uid)) {
                        list.push(uid);
                    }
                }
                Ok(())
            };
        for (group_id, ids) in add {
            request(group_id, ids, true)?;
        }
        for (group_id, ids) in remove {
            request(group_id, ids, false)?;
        }
        for (from, to, ids) in moves {
            if from == to {
                return Err(ContactsError::invalid("a move needs two different groups"));
            }
            request(to, ids, true)?;
            request(from, ids, false)?;
        }
        if changes.is_empty() {
            return Err(ContactsError::invalid("give add, remove or move"));
        }
        // Adds must all land before any removal. A group that both gains and
        // loses members cannot be ordered that way once other groups change
        // too (a swap or a chain of moves), so such calls are split.
        if changes.len() > 1
            && let Some((group_id, _)) = changes
                .iter()
                .find(|(_, (adding, removing))| !adding.is_empty() && !removing.is_empty())
        {
            return Err(ContactsError::coded(
                "split_required",
                format!(
                    "{group_id} both gains and loses members while other groups change too, so no order keeps every contact listed if a write fails. Send one call with only the additions (every destination), then one with only the removals (every source)."
                ),
            ));
        }
        // Groups gaining members first: a refused write stops the rest, so a
        // move never leaves a contact in neither group.
        let mut ordered: Vec<(&String, &MemberChange)> =
            changes.iter().filter(|(_, (a, _))| !a.is_empty()).collect();
        ordered.extend(changes.iter().filter(|(_, (a, _))| a.is_empty()));
        let mut plan = Plan::new("updated", true);
        for (group_id, (adding, removing)) in ordered {
            let (book_id, resource) = split_id(group_id)?;
            let info = Self::book_info(&snapshot, book_id)?;
            let fetched = self.fetch_current(&info, resource, group_id, None).await?;
            let edited = edit::change_members(&fetched.card, adding, removing)?;
            if edited.unchanged() {
                plan.unchanged.push(group_id.clone());
                continue;
            }
            let purpose = match (adding.is_empty(), removing.is_empty()) {
                (false, true) => "add members",
                (true, false) => "remove members",
                _ => "change members",
            };
            plan.steps.push(Self::put_step(
                group_id.clone(),
                info.resource_url(resource)?,
                purpose,
                Some(fetched),
                edited,
            ));
        }
        if plan.steps.is_empty() {
            plan.status = "unchanged";
        }
        Ok(plan)
    }

    /// Plans a write without sending anything.
    pub async fn preview(
        &self,
        action: &Action,
        key: Option<&str>,
    ) -> Result<PreviewOutput, ContactsError> {
        // Planning holds several cards across awaits; keep it off the stack.
        Box::pin(self.preview_inner(action, key)).await
    }

    async fn preview_inner(
        &self,
        action: &Action,
        key: Option<&str>,
    ) -> Result<PreviewOutput, ContactsError> {
        let seed = key.unwrap_or("preview-without-a-key");
        let plan = self.plan(action, seed).await?;
        let mut warnings = plan.warnings.clone();
        if key.is_none()
            && matches!(
                action,
                Action::GroupCreate { .. } | Action::ContactCreate { .. }
            )
        {
            warnings.push(
                "the new id depends on the idempotencyKey; pass it to preview the exact id"
                    .to_owned(),
            );
        }
        Ok(PreviewOutput {
            status: plan.status.to_owned(),
            contact_id: plan.contact_id.clone(),
            group_id: plan.group_id.clone(),
            writes: plan
                .steps
                .iter()
                .map(|step| {
                    let (removed_lines, added_lines) =
                        line_diff(step.before.as_ref().map(|f| &f.card), step.after.as_ref());
                    PlannedWrite {
                        method: step.kind.method().to_owned(),
                        resource_id: step.resource_id.clone(),
                        purpose: step.purpose.to_owned(),
                        precondition: step.precondition.describe(),
                        changes: step.changes.clone(),
                        removed_lines,
                        added_lines,
                    }
                })
                .collect(),
            unchanged: plan.unchanged.clone(),
            warnings,
        })
    }

    async fn load(&self, key_hash: &str) -> Result<Option<OperationRecord>, ContactsError> {
        let key = key_hash.to_owned();
        Ok(self
            .inner
            .store
            .read(move |docs| docs.get::<OperationRecord>(&key))
            .await?)
    }

    async fn save(&self, record: &OperationRecord) -> Result<(), ContactsError> {
        let record = record.clone();
        let ttl = record.ttl_ms();
        self.inner
            .store
            .write(move |tx| {
                tx.upsert(
                    &record,
                    UpsertOpts {
                        expires_at: None,
                        ttl_ms: ttl,
                    },
                )
            })
            .await?;
        Ok(())
    }

    async fn keep_recovery_copy(
        &self,
        key_hash: &str,
        index: usize,
        resource_id: &str,
        before: &Fetched,
    ) -> Result<(), ContactsError> {
        let copy = RecoveryCopy {
            key: recovery_key(key_hash, index),
            resource_id: resource_id.to_owned(),
            etag: before.etag.clone(),
            vcard: before.text.clone(),
            created_at: self.now_ms(),
            extra: Default::default(),
        };
        self.inner
            .store
            .write(move |tx| {
                tx.upsert(
                    &copy,
                    UpsertOpts {
                        expires_at: None,
                        ttl_ms: Some(RECOVERY_TTL_MS),
                    },
                )
            })
            .await?;
        Ok(())
    }

    async fn drop_recovery_copy(&self, key_hash: &str, index: usize) {
        let key = recovery_key(key_hash, index);
        if let Err(error) = self
            .inner
            .store
            .write(move |tx| tx.delete::<RecoveryCopy>(&key).map(|_| ()))
            .await
        {
            tracing::warn!(target: LOG, "Could not drop a contacts recovery copy: {error}");
        }
    }

    /// Runs `work` so a dropped caller cannot cut it short (with a tracker).
    async fn detached<T: Send + 'static>(
        &self,
        work: impl std::future::Future<Output = T> + Send + 'static,
    ) -> T {
        match &self.inner.tracker {
            Some(tracker) => omni_core::spawn::must_complete(tracker, work).await,
            None => work.await,
        }
    }

    /// Runs a write tool call. The write lock is an owned guard moved into
    /// the detached work, so a caller that goes away mid-write cannot let
    /// another write (or a replay of this key) start before it settles.
    pub async fn execute(&self, request: WriteRequest) -> Result<WriteOutput, ContactsError> {
        Box::pin(self.execute_inner(request)).await
    }

    async fn execute_inner(&self, request: WriteRequest) -> Result<WriteOutput, ContactsError> {
        if !is_valid_key(&request.idempotency_key) {
            return Err(ContactsError::invalid(
                "idempotencyKey must be 16-128 characters of A-Z, a-z, 0-9, _ or -",
            ));
        }
        self.directory()?;
        let key_hash = sha256_hex(request.idempotency_key.as_bytes());
        let guard = self.inner.write_lock.clone().lock_owned().await;
        if let Some(existing) = self.load(&key_hash).await? {
            if existing.fingerprint != request.fingerprint || existing.tool != request.tool {
                return Err(ContactsError::coded(
                    "idempotency_key_reused",
                    "this idempotencyKey was used for a different request; use a new key",
                ));
            }
            if existing.state == OperationState::Reserved {
                return Err(ContactsError::coded(
                    "in_progress",
                    "this write has not finished; check contacts_write_status",
                ));
            }
            let service = self.clone();
            let record = self
                .detached(async move {
                    let _guard = guard;
                    service.settle(existing).await
                })
                .await?;
            return outcome(&record, true);
        }
        let plan = self.plan(&request.action, &request.idempotency_key).await?;
        let now = self.now_ms();
        let summary = Summary {
            status: plan.status.to_owned(),
            contact_id: plan.contact_id.clone(),
            group_id: plan.group_id.clone(),
            unchanged: plan.unchanged.clone(),
            warnings: plan.warnings.clone(),
        };
        let record = OperationRecord {
            key_hash: key_hash.clone(),
            idempotency_key: request.idempotency_key.clone(),
            fingerprint: request.fingerprint.clone(),
            tool: request.tool.to_owned(),
            state: OperationState::Reserved,
            steps: plan
                .steps
                .iter()
                .map(|step| OperationStep {
                    kind: step.kind,
                    resource_id: step.resource_id.clone(),
                    purpose: step.purpose.to_owned(),
                    precondition: step.precondition.describe(),
                    before_etag: step.before.as_ref().map(|f| f.etag.clone()),
                    body_sha256: step.body.as_deref().map(|b| sha256_hex(b.as_bytes())),
                    state: StepState::Planned,
                    put_etag: None,
                    result_etag: None,
                    http_status: None,
                    detail: None,
                    changes: step.changes.clone(),
                    expect: step.expect.clone(),
                    verified: false,
                })
                .collect(),
            dependent: plan.dependent,
            result: serde_json::to_value(&summary).ok(),
            error: None,
            created_at: now,
            updated_at: now,
            extra: Default::default(),
        };
        let service = self.clone();
        let record = self
            .detached(async move {
                let _guard = guard;
                service.reserve_and_run(record, plan).await
            })
            .await?;
        outcome(&record, false)
    }

    async fn reserve_and_run(
        &self,
        mut record: OperationRecord,
        plan: Plan,
    ) -> Result<OperationRecord, ContactsError> {
        if plan.steps.is_empty() {
            record.state = OperationState::Confirmed;
            self.save(&record).await?;
            return Ok(record);
        }
        self.save(&record).await?;
        let result = self.run(record, plan).await;
        if let Ok(directory) = self.directory() {
            directory.invalidate().await;
        }
        result
    }

    async fn run(
        &self,
        mut record: OperationRecord,
        plan: Plan,
    ) -> Result<OperationRecord, ContactsError> {
        let directory = self.directory()?;
        let dav = directory.dav();
        let mut blocked = false;
        for (index, step) in plan.steps.iter().enumerate() {
            if blocked {
                record.steps[index].state = StepState::Skipped;
                record.steps[index].detail =
                    Some("not sent: an earlier write of this request did not complete".to_owned());
                continue;
            }
            if let Some(before) = &step.before {
                self.keep_recovery_copy(&record.key_hash, index, &step.resource_id, before)
                    .await?;
            }
            record.steps[index].state = StepState::Sending;
            record.updated_at = self.now_ms();
            self.save(&record).await?;
            let sent = match (step.kind, &step.body, &step.precondition) {
                (StepKind::Put, Some(body), precondition) => {
                    dav.put(&step.url, body.clone(), precondition).await
                }
                (StepKind::Delete, _, Precondition::IfMatch(etag)) => {
                    dav.delete(&step.url, etag).await
                }
                _ => Err(WriteError::NotSent("invalid plan step".to_owned())),
            };
            match classify(&sent) {
                Sent::Applied { status, etag } => {
                    let current = &mut record.steps[index];
                    current.http_status = Some(status);
                    current.put_etag = etag.clone();
                    current.result_etag = etag;
                    current.state = StepState::Acknowledged;
                    if dav.is_recording() {
                        current.state = StepState::Verified;
                        current.detail = Some("recorded, not sent".to_owned());
                    } else {
                        self.verify(&mut record.steps[index], step).await;
                    }
                    match record.steps[index].state {
                        StepState::Verified if step.kind == StepKind::Put => {
                            self.drop_recovery_copy(&record.key_hash, index).await;
                        }
                        // The next card would likely lose the same data:
                        // stop every later write, dependent or not.
                        StepState::Diverged => blocked = true,
                        _ => {}
                    }
                }
                Sent::Rejected {
                    status,
                    code,
                    message,
                } => {
                    if code == "unauthorized" {
                        directory.forget().await;
                    }
                    let current = &mut record.steps[index];
                    current.http_status = status;
                    current.state = StepState::Rejected;
                    current.detail = Some(format!("[{code}] {message}"));
                    if record.error.is_none() {
                        record.error = Some(OperationErrorRecord {
                            code: code.to_owned(),
                            message: format!("{}: {message}", step.resource_id),
                        });
                    }
                    self.drop_recovery_copy(&record.key_hash, index).await;
                    if plan.dependent {
                        blocked = true;
                    }
                }
                Sent::Uncertain { status, message } => {
                    tracing::warn!(target: LOG, "Contacts write outcome uncertain: {message}");
                    let current = &mut record.steps[index];
                    current.http_status = status;
                    current.state = StepState::Uncertain;
                    current.detail = Some(message);
                    // Never resent; nothing after it is sent either.
                    blocked = true;
                }
            }
            record.updated_at = self.now_ms();
            self.save(&record).await?;
        }
        let mut record = self.settle(record).await?;
        if record.state == OperationState::Failed && record.error.is_none() {
            record.error = Some(OperationErrorRecord {
                code: "failed".to_owned(),
                message: "no write applied".to_owned(),
            });
            self.save(&record).await?;
        }
        Ok(record)
    }

    /// Reads a card fresh for verification: `Ok(None)` when it is gone,
    /// `Err` when it cannot be read or parsed (the step stays as it is).
    async fn read_back(&self, url: &Url) -> Result<Option<(String, Card)>, String> {
        let directory = self.directory().map_err(|e| e.to_string())?;
        let response = directory.dav().get(url).await.map_err(|e| e.to_string())?;
        if response.status == 404 {
            return Ok(None);
        }
        if !response.is_success() {
            return Err(format!("HTTP {}", response.status));
        }
        let etag = response.etag.clone().ok_or_else(|| "no ETag".to_owned())?;
        let text = response.utf8().map_err(|_| "not UTF-8".to_owned())?;
        let card = Card::parse(text).map_err(|e| e.to_string())?;
        Ok(Some((etag, card)))
    }

    /// Reads a written card back and compares it with the plan.
    async fn verify(&self, step: &mut OperationStep, planned: &PlannedStep) {
        match self.read_back(&planned.url).await {
            Ok(read) => judge(step, read),
            Err(error) => step.detail = Some(format!("not read back: {error}")),
        }
    }

    /// Settles uncertain and unverified steps by reading, then the state.
    /// A step is settled as not applied only when the card provably still
    /// is what the write was planned against; nothing is ever resent.
    async fn settle(&self, mut record: OperationRecord) -> Result<OperationRecord, ContactsError> {
        let open = record
            .steps
            .iter()
            .any(|s| matches!(s.state, StepState::Uncertain | StepState::Acknowledged));
        if open && let Ok(directory) = self.directory() {
            let snapshot = directory.snapshot(false).await.ok();
            let elapsed = self.now_ms() - record.created_at;
            for (index, step) in record.steps.iter_mut().enumerate() {
                if !matches!(step.state, StepState::Uncertain | StepState::Acknowledged) {
                    continue;
                }
                let Ok((book_id, resource)) = split_id(&step.resource_id) else {
                    continue;
                };
                let Some(info) = snapshot
                    .as_ref()
                    .and_then(|s| s.book(book_id))
                    .map(|b| b.info.clone())
                else {
                    continue;
                };
                let Ok(url) = info.resource_url(resource) else {
                    continue;
                };
                let Ok(read) = self.read_back(&url).await else {
                    continue;
                };
                if step.state == StepState::Uncertain {
                    settle_uncertain(step, read, elapsed >= SETTLE_AFTER_MS);
                } else {
                    judge(step, read);
                }
                // A deleted card's copy stays until its TTL: it is the only
                // record of what was deleted.
                if step.state == StepState::Verified && step.kind == StepKind::Put {
                    let key_hash = record.key_hash.clone();
                    self.drop_recovery_copy(&key_hash, index).await;
                }
            }
        }
        record.state = record.settled_state();
        record.updated_at = self.now_ms();
        self.save(&record).await?;
        Ok(record)
    }

    pub async fn write_status(&self, key: &str) -> Result<WriteStatusOutput, ContactsError> {
        if !is_valid_key(key) {
            return Err(ContactsError::invalid("invalid idempotencyKey"));
        }
        let key_hash = sha256_hex(key.as_bytes());
        let guard = self.inner.write_lock.clone().lock_owned().await;
        let Some(record) = self.load(&key_hash).await? else {
            return Ok(WriteStatusOutput {
                found: false,
                state: None,
                tool: None,
                created_at: None,
                updated_at: None,
                result: None,
                error: None,
            });
        };
        let record = if record.state == OperationState::Reserved {
            record
        } else {
            let service = self.clone();
            self.detached(async move {
                let _guard = guard;
                service.settle(record).await
            })
            .await?
        };
        Ok(WriteStatusOutput {
            found: true,
            state: Some(record.state.as_str().to_owned()),
            tool: Some(record.tool.clone()),
            created_at: Some(ms_text(record.created_at)),
            updated_at: Some(ms_text(record.updated_at)),
            result: Some(output(&record, true)),
            error: record.error.as_ref().map(|e| WriteErrorOut {
                code: e.code.clone(),
                message: e.message.clone(),
            }),
        })
    }
}

fn check_detail(result: &edit::Check) -> String {
    let mut parts = Vec::new();
    if !result.lost.is_empty() {
        parts.push(format!(
            "unedited properties not kept: {}",
            result.lost.join(", ")
        ));
    }
    if !result.missing.is_empty() {
        parts.push(format!(
            "written lines missing: {}",
            result.missing.join(", ")
        ));
    }
    if !result.lingering.is_empty() {
        parts.push(format!(
            "removed lines still present: {}",
            result.lingering.join(", ")
        ));
    }
    parts.join("; ")
}

/// Whether an ETag is strong (`W/` marks a weak one).
fn is_strong(etag: &str) -> bool {
    !etag.trim_start().starts_with("W/")
}

/// Whether two ETags name the same version. Surrounding quotes are ignored
/// (a listing's `getetag` and a GET `ETag` header may quote differently),
/// but weakness is not: a weak and a strong ETag are never the same.
pub fn same_etag(a: &str, b: &str) -> bool {
    let parts = |e: &str| {
        let e = e.trim();
        match e.strip_prefix("W/") {
            Some(rest) => (true, rest.trim_matches('"').to_owned()),
            None => (false, e.trim_matches('"').to_owned()),
        }
    };
    parts(a) == parts(b)
}

/// Judges an acknowledged write from a fresh read of its card.
///
/// A read still showing the planned-against version leaves the step waiting
/// for the write to become visible. Any other read that does not match the
/// plan is a preservation failure (`diverged`), unless there is positive
/// evidence that a later version from another client is being read: a strong
/// ETag on the PUT answer that differs from a strong ETag on the read. RFC 6352 6.3.2.3 lets
/// a server that stores other octets than were sent answer with no ETag or a
/// weak one, so a missing or weak PUT ETag never excuses a mismatch.
fn judge(step: &mut OperationStep, read: Option<(String, Card)>) {
    match (step.kind, read) {
        (StepKind::Delete, None) => {
            step.state = StepState::Verified;
            step.verified = true;
            step.detail = None;
        }
        (StepKind::Delete, Some((etag, _))) => {
            step.state = StepState::Acknowledged;
            step.detail = Some(
                if step
                    .before_etag
                    .as_deref()
                    .is_some_and(|b| same_etag(b, &etag))
                {
                    "the card still shows the deleted version; the delete is not visible yet"
                        .to_owned()
                } else {
                    "a newer version of the card exists; it changed elsewhere".to_owned()
                },
            );
        }
        (StepKind::Put, None) => {
            step.state = StepState::Acknowledged;
            step.detail = Some("the card was not found when read back".to_owned());
        }
        (StepKind::Put, Some((etag, card))) => {
            let Some(expect) = &step.expect else {
                return;
            };
            if step
                .before_etag
                .as_deref()
                .is_some_and(|b| same_etag(b, &etag))
            {
                step.state = StepState::Acknowledged;
                step.detail = Some(
                    "the card still shows its earlier version; the write is not visible yet"
                        .to_owned(),
                );
                return;
            }
            let result = edit::check(expect, &card);
            let later_version = step
                .put_etag
                .as_deref()
                .is_some_and(|put| is_strong(put) && is_strong(&etag) && !same_etag(put, &etag));
            step.result_etag = Some(etag);
            if result.clean() {
                step.state = StepState::Verified;
                step.verified = true;
                step.detail = None;
            } else if later_version {
                step.state = StepState::Acknowledged;
                step.detail = Some(format!(
                    "the card changed elsewhere after this write (its version differs from the write's strong ETag), so differences cannot be attributed: {}",
                    check_detail(&result)
                ));
            } else {
                step.state = StepState::Diverged;
                let evidence = match step.put_etag.as_deref() {
                    Some(put) if is_strong(put) => "",
                    Some(_) => " (iCloud answered the write with a weak ETag)",
                    None => " (iCloud answered the write without an ETag)",
                };
                step.detail = Some(format!("{}{evidence}", check_detail(&result)));
            }
        }
    }
}

/// Settles a request whose answer was lost. Not applied only when the card
/// provably still is what the write was planned against (same ETag, or a
/// create whose card still does not exist) after the settle delay; applied
/// when the card shows the write; otherwise it stays uncertain.
fn settle_uncertain(step: &mut OperationStep, read: Option<(String, Card)>, settle: bool) {
    let original = match (&read, &step.before_etag) {
        (Some((etag, _)), Some(before)) => same_etag(etag, before),
        (None, None) => step.kind == StepKind::Put,
        _ => false,
    };
    if original {
        if settle {
            step.state = StepState::Rejected;
            step.detail = Some(
                "not applied: the card is still the version it was planned against".to_owned(),
            );
        }
        return;
    }
    let shows_write = match (step.kind, &read) {
        (StepKind::Delete, None) => true,
        (StepKind::Put, Some((_, card))) => step
            .expect
            .as_ref()
            .is_some_and(|e| edit::check(e, card).applied()),
        _ => false,
    };
    if shows_write {
        judge(step, read);
    } else {
        step.detail = Some(
            "the card changed elsewhere and does not show this write; whether it applied is unknown, and it is never resent"
                .to_owned(),
        );
    }
}

fn output(record: &OperationRecord, replayed: bool) -> WriteOutput {
    let summary: Summary = record
        .result
        .clone()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    let mut warnings = summary.warnings.clone();
    match record.state {
        OperationState::Uncertain => warnings.push(
            "a write's outcome is unknown; check contacts_write_status and do not retry with a new key".to_owned(),
        ),
        OperationState::Unverified => warnings.push(
            "writes were acknowledged but not read back; check contacts_write_status".to_owned(),
        ),
        _ => {}
    }
    WriteOutput {
        status: summary.status,
        state: record.state.as_str().to_owned(),
        contact_id: summary.contact_id,
        group_id: summary.group_id,
        steps: record
            .steps
            .iter()
            .map(|s| StepOut {
                resource_id: s.resource_id.clone(),
                method: s.kind.method().to_owned(),
                purpose: s.purpose.clone(),
                state: s.state.as_str().to_owned(),
                http_status: s.http_status,
                etag: s.result_etag.clone(),
                verified: s.verified,
                changes: s.changes.clone(),
                detail: s.detail.clone(),
            })
            .collect(),
        unchanged: summary.unchanged,
        verified: !record.steps.is_empty() && record.steps.iter().all(|s| s.verified),
        warnings,
        replayed,
    }
}

/// The tool result of a settled operation: failed and diverged operations
/// are errors (nothing applied, or applied without keeping the card intact).
fn outcome(record: &OperationRecord, replayed: bool) -> Result<WriteOutput, ContactsError> {
    match record.state {
        OperationState::Failed => {
            let error = record.error.clone().unwrap_or(OperationErrorRecord {
                code: "failed".to_owned(),
                message: "no write applied".to_owned(),
            });
            let code = match error.code.as_str() {
                "version_conflict" => "version_conflict",
                "not_found" => "not_found",
                "unauthorized" => "unauthorized",
                "not_sent" => "not_sent",
                "rejected" => "rejected",
                _ => "failed",
            };
            Err(ContactsError::coded(
                code,
                format!("nothing was changed: {}", error.message),
            ))
        }
        OperationState::Diverged => {
            let details: Vec<String> = record
                .steps
                .iter()
                .filter(|s| s.state == StepState::Diverged)
                .map(|s| {
                    format!(
                        "{} ({})",
                        s.resource_id,
                        s.detail.clone().unwrap_or_default()
                    )
                })
                .collect();
            Err(ContactsError::coded(
                "data_not_preserved",
                format!(
                    "a write applied but the card read back does not match the plan: {}. Nothing will be retried; contacts_write_status shows every step",
                    details.join("; ")
                ),
            ))
        }
        _ => Ok(output(record, replayed)),
    }
}

fn detail(
    contact_id: &str,
    book_id: &str,
    etag: &str,
    view: &Projection,
    groups: Vec<GroupRef>,
    blockers: Vec<String>,
) -> ContactDetail {
    ContactDetail {
        contact_id: contact_id.to_owned(),
        book_id: book_id.to_owned(),
        etag: etag.to_owned(),
        uid: view.uid.clone(),
        display_name: view.display_name(),
        full_name: view.full_name.clone(),
        name: view.name.as_ref().map(|n| NameOut {
            family: n.family.clone(),
            given: n.given.clone(),
            middle: n.middle.clone(),
            prefix: n.prefix.clone(),
            suffix: n.suffix.clone(),
        }),
        nickname: view.nickname.clone(),
        organization: view.organization.clone(),
        department: view.department.clone(),
        job_title: view.job_title.clone(),
        birthday: view.birthday.clone(),
        birthday_year_omitted: view.birthday_year_omitted,
        note: view.note.clone(),
        values: view
            .items
            .iter()
            .map(|item| ValueOut {
                id: item.id.clone(),
                field: value_field(item.field),
                value: item.value.clone(),
                label: item.label.clone(),
                label_token: item.label_token.clone(),
                custom_label: item.custom_label,
                preferred: item.preferred,
                address: item.address.as_ref().map(|a| AddressOut {
                    street: a.street.clone(),
                    city: a.city.clone(),
                    region: a.region.clone(),
                    postal_code: a.postal_code.clone(),
                    country: a.country.clone(),
                    po_box: a.po_box.clone(),
                    extended: a.extended.clone(),
                    country_code: a.country_code.clone(),
                }),
                user: item.user.clone(),
                service: item.service.clone(),
            })
            .collect(),
        groups,
        other_properties: view
            .other
            .iter()
            .map(|(name, count)| PropertyCount {
                name: name.clone(),
                count: *count,
            })
            .collect(),
        has_photo: view.has_photo,
        vcard_version: view.version.clone(),
        editable: blockers.is_empty(),
        blockers,
    }
}

/// Marks writes interrupted by a restart: sent ones uncertain, unsent failed.
pub async fn reconcile_interrupted(
    store: &Store,
    now_ms: i64,
) -> Result<u64, omni_store::StoreError> {
    store
        .write(move |tx| {
            let mut changed = 0u64;
            for mut record in tx.get_all::<OperationRecord>()? {
                if record.state != OperationState::Reserved {
                    continue;
                }
                let sent = record.steps.iter().any(|s| s.state != StepState::Planned);
                for step in &mut record.steps {
                    if step.state == StepState::Sending {
                        step.state = StepState::Uncertain;
                    }
                }
                if sent {
                    record.state = record.settled_state();
                } else {
                    record.state = OperationState::Failed;
                    record.error = Some(OperationErrorRecord {
                        code: "interrupted".to_owned(),
                        message: "the write was interrupted before anything was sent".to_owned(),
                    });
                }
                record.updated_at = now_ms;
                let ttl = record.ttl_ms();
                tx.upsert(
                    &record,
                    UpsertOpts {
                        expires_at: None,
                        ttl_ms: ttl,
                    },
                )?;
                changed += 1;
            }
            Ok(changed)
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    const BEFORE: &str = "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:A\r\nX-KEEP:1\r\nEND:VCARD\r\n";
    const PLANNED: &str =
        "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:A\r\nNICKNAME:B\r\nX-KEEP:1\r\nEND:VCARD\r\n";
    const LOSSY: &str = "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:A\r\nNICKNAME:B\r\nEND:VCARD\r\n";

    fn step(put_etag: Option<&str>) -> OperationStep {
        let before = Card::parse(BEFORE).unwrap();
        let planned = Card::parse(PLANNED).unwrap();
        OperationStep {
            kind: StepKind::Put,
            resource_id: "card/a.vcf".to_owned(),
            purpose: "update contact".to_owned(),
            precondition: "if-match:\"e1\"".to_owned(),
            before_etag: Some("\"e1\"".to_owned()),
            body_sha256: None,
            state: StepState::Acknowledged,
            put_etag: put_etag.map(str::to_owned),
            result_etag: None,
            http_status: Some(204),
            detail: None,
            changes: Vec::new(),
            expect: Some(edit::expectation(Some(&before), &planned)),
            verified: false,
        }
    }

    fn judged(put: Option<&str>, read: &str, body: &str) -> StepState {
        let mut step = step(put);
        judge(
            &mut step,
            Some((read.to_owned(), Card::parse(body).unwrap())),
        );
        step.state
    }

    #[test]
    fn only_two_strong_differing_etags_excuse_a_mismatch() {
        // A later edit elsewhere is believed only with strong evidence.
        assert_eq!(
            judged(Some("\"e2\""), "\"e3\"", LOSSY),
            StepState::Acknowledged
        );
        // Weak, missing or quote-only differences never excuse lost data.
        for (put, read) in [
            (None, "\"e3\""),
            (Some("W/\"e2\""), "\"e3\""),
            (Some("W/\"e2\""), "W/\"e2\""),
            (Some("\"e2\""), "W/\"e3\""),
            (Some("\"e2\""), "e2"),
            (Some("\"e2\""), "\"e2\""),
        ] {
            assert_eq!(
                judged(put, read, LOSSY),
                StepState::Diverged,
                "{put:?} {read}"
            );
        }
        // A match is verified whatever the ETags say.
        assert_eq!(
            judged(Some("W/\"e2\""), "W/\"e2\"", PLANNED),
            StepState::Verified
        );
        // The planned-against version still visible: not yet visible, no claim.
        assert_eq!(judged(None, "e1", BEFORE), StepState::Acknowledged);
    }

    #[test]
    fn weakness_is_part_of_an_etag() {
        assert!(same_etag("\"C=1@U=2\"", "C=1@U=2"));
        assert!(same_etag("W/\"x\"", "W/x"));
        assert!(!same_etag("W/\"x\"", "\"x\""));
        assert!(!same_etag("\"x\"", "\"y\""));
    }
}
