//! The app's state: the phone's copy of the library, the sync with Omni, and
//! the outbox every write goes through. Screens read the signals and call the
//! methods; nothing else talks to the network or to IndexedDB.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;
use omni_api::common::Ms;
use omni_api::fond::{
    AppendCookEventsRequest, ClientOp, ClientOpKind, CookEventKind, CookSession, CookState, Hello,
    ImageVariant, InboxItem, InboxList, KitchenRequest, OpsResponse, Original, RecipeRecord,
    RecipeState, RequestList, RequestOrigin, SessionChanged, StartCookRequest, SyncResponse, Tag,
    paths, sse,
};
use omni_recipe_format::RecipeDoc;
use serde::{Deserialize, Serialize};
use wasm_bindgen::JsCast;

use crate::logic::library::{self, Filter, Sort};
use crate::logic::outbox::{self, Batch, Entry, Job, Outcome};
use crate::logic::session::{self, Projection};
use crate::logic::{Route, cover};
use crate::platform::net::{self, NetError};
use crate::platform::{device, idb, listen, now_ms, random_id, visible, window};

/// One recipe as the screens use it: the record, its decoded document and
/// what the library needs of it.
#[derive(Debug, Clone)]
pub struct Recipe {
    pub rec: RecipeRecord,
    /// None when the document is in a format this build cannot read.
    pub doc: Option<Arc<RecipeDoc>>,
    pub title: String,
    pub item: library::Item,
}

impl Recipe {
    fn new(rec: RecipeRecord) -> Recipe {
        let doc = omni_recipe_format::upgrade(rec.format, rec.doc.clone()).ok();
        let title = doc.as_ref().map_or_else(
            || {
                rec.doc
                    .get("title")
                    .and_then(|title| title.as_str())
                    .unwrap_or("Untitled")
                    .to_owned()
            },
            |doc| doc.title.clone(),
        );
        let item = library::item(&rec, doc.as_ref(), &title);
        Recipe {
            rec,
            doc: doc.map(Arc::new),
            title,
            item,
        }
    }
}

/// What a toast's Undo runs. Not a `Callback`: that would be disposed with
/// the screen that raised the toast, and the toast outlives it.
pub type Undo = Arc<dyn Fn() + Send + Sync>;

#[derive(Clone)]
pub struct Toast {
    pub id: u64,
    pub text: String,
    pub undo: Option<Undo>,
}

/// The notice a screen raises when what it shows has changed elsewhere and
/// it will not swap the content by itself (a sheet or an editor is open, or a
/// cook is under way). `reload` shows the new content.
#[derive(Clone)]
pub struct Stale {
    pub id: u64,
    pub text: &'static str,
    pub reload: Undo,
}

/// What a request for the kitchen agent is about.
#[derive(Debug, Clone, PartialEq)]
pub struct Ask {
    pub origin: RequestOrigin,
    pub recipe_id: Option<String>,
    pub revision: Option<u32>,
    pub scale: Option<f64>,
    pub step: Option<String>,
    /// The line the sheet shows: "About Dad's Mustard Vinaigrette".
    pub about: String,
}

impl Ask {
    pub fn library() -> Ask {
        Ask {
            origin: RequestOrigin::Library,
            recipe_id: None,
            revision: None,
            scale: None,
            step: None,
            about: "About your library, or a recipe to add.".to_owned(),
        }
    }
}

/// A timer that has run out and not been acknowledged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    pub session: String,
    pub step: String,
    pub ends_at: Ms,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Meta {
    seq: Option<u64>,
    build: Option<String>,
    sort: Option<String>,
    inbox_pending: u32,
}

#[derive(Clone, Copy)]
pub struct Store {
    /// The stored library has been read; screens can render.
    pub ready: RwSignal<bool>,
    /// At least one sync has ever succeeded on this phone.
    pub has_library: RwSignal<bool>,
    pub recipes: RwSignal<BTreeMap<String, Arc<Recipe>>>,
    pub tags: RwSignal<Vec<Tag>>,
    pub sessions: RwSignal<BTreeMap<String, CookSession>>,
    pinned: RwSignal<BTreeMap<String, (u32, Arc<RecipeDoc>)>>,
    pub scales: RwSignal<BTreeMap<String, f64>>,
    pub outbox: RwSignal<Vec<Entry>>,
    pub inbox: RwSignal<Vec<InboxItem>>,
    pub inbox_pending: RwSignal<u32>,
    /// The requests the server has, newest first.
    pub requests: RwSignal<Vec<KitchenRequest>>,
    pub stale: RwSignal<Option<Stale>>,
    /// A notes field has the keyboard.
    pub editing: RwSignal<bool>,
    /// The window's width in CSS pixels.
    pub width: RwSignal<f64>,
    /// The request being written. It is here, not in the sheet, so a screen
    /// that is redrawn under the sheet does not lose it.
    pub draft: RwSignal<String>,
    /// Photos being prepared for upload: `(done, total)`.
    pub preparing: RwSignal<Option<(usize, usize)>>,
    pub online: RwSignal<bool>,
    pub update_available: RwSignal<bool>,
    pub route: RwSignal<Route>,
    /// The wall clock, four times a second. Timers are end times compared
    /// with it, so a locked screen or a suspended app loses nothing.
    pub now: RwSignal<Ms>,
    pub toast: RwSignal<Option<Toast>>,
    pub alerts: RwSignal<Vec<Alert>>,
    pub filter: RwSignal<Filter>,
    pub sort: RwSignal<Sort>,
    pub library_scroll: RwSignal<f64>,
    pub wake_refused: RwSignal<bool>,
    meta: StoredValue<Meta>,
    in_flight: StoredValue<Vec<u64>>,
    pumping: StoredValue<bool>,
    syncing: StoredValue<bool>,
    sync_again: StoredValue<bool>,
    failures: StoredValue<u32>,
    next_key: StoredValue<u64>,
    notes_timer: StoredValue<Option<TimeoutHandle>>,
    toast_seq: StoredValue<u64>,
    requests_asked: StoredValue<u64>,
    stale_seq: StoredValue<u64>,
    last_wake: StoredValue<Ms>,
    last_ring: StoredValue<Ms>,
}

thread_local! {
    static EVENTS: RefCell<Option<web_sys::EventSource>> = const { RefCell::new(None) };
}

const NOTES_DEBOUNCE: Duration = Duration::from_millis(800);
pub const UNDO_WINDOW: Duration = Duration::from_secs(10);
/// A timer that ran out rings again this often, for this long.
const RING_EVERY_MS: Ms = 10_000;
const RING_FOR_MS: Ms = 60_000;
/// The long edge of an uploaded photo.
const PHOTO_EDGE: u32 = 2400;

pub fn use_store() -> Store {
    expect_context::<Store>()
}

impl Store {
    pub fn new() -> Store {
        let path = window().location().pathname().unwrap_or_default();
        Store {
            ready: RwSignal::new(false),
            has_library: RwSignal::new(false),
            recipes: RwSignal::new(BTreeMap::new()),
            tags: RwSignal::new(Vec::new()),
            sessions: RwSignal::new(BTreeMap::new()),
            pinned: RwSignal::new(BTreeMap::new()),
            scales: RwSignal::new(BTreeMap::new()),
            outbox: RwSignal::new(Vec::new()),
            inbox: RwSignal::new(Vec::new()),
            inbox_pending: RwSignal::new(0),
            requests: RwSignal::new(Vec::new()),
            stale: RwSignal::new(None),
            editing: RwSignal::new(false),
            width: RwSignal::new(window_width()),
            draft: RwSignal::new(String::new()),
            preparing: RwSignal::new(None),
            online: RwSignal::new(window().navigator().on_line()),
            update_available: RwSignal::new(false),
            route: RwSignal::new(Route::parse(&path)),
            now: RwSignal::new(now_ms()),
            toast: RwSignal::new(None),
            alerts: RwSignal::new(Vec::new()),
            filter: RwSignal::new(Filter::default()),
            sort: RwSignal::new(Sort::default()),
            library_scroll: RwSignal::new(0.0),
            wake_refused: RwSignal::new(false),
            meta: StoredValue::new(Meta::default()),
            in_flight: StoredValue::new(Vec::new()),
            pumping: StoredValue::new(false),
            syncing: StoredValue::new(false),
            sync_again: StoredValue::new(false),
            failures: StoredValue::new(0),
            next_key: StoredValue::new(1),
            notes_timer: StoredValue::new(None),
            toast_seq: StoredValue::new(0),
            requests_asked: StoredValue::new(0),
            stale_seq: StoredValue::new(0),
            last_wake: StoredValue::new(0),
            last_ring: StoredValue::new(0),
        }
    }

    /// Reads the stored library, then starts syncing and listening.
    pub fn start(self) {
        spawn_local(async move {
            let records: Vec<RecipeRecord> = idb::all(idb::RECIPES).await;
            let sessions: Vec<CookSession> = idb::all(idb::SESSIONS).await;
            let entries: Vec<Entry> = idb::all(idb::OUTBOX).await;
            let meta: Meta = idb::get(idb::KV, "meta").await.unwrap_or_default();
            let tags: Vec<Tag> = idb::get(idb::KV, "tags").await.unwrap_or_default();
            let scales: BTreeMap<String, f64> =
                idb::get(idb::KV, "scales").await.unwrap_or_default();
            let inbox: Vec<InboxItem> = idb::get(idb::KV, "inbox").await.unwrap_or_default();
            let requests: Vec<KitchenRequest> =
                idb::get(idb::KV, "requests").await.unwrap_or_default();
            let pinned = sessions
                .iter()
                .filter_map(|session| {
                    Some((
                        session.id.clone(),
                        (session.revision, decode_pinned(session)?),
                    ))
                })
                .collect();
            let mut entries = entries;
            let mut next_key = entries.last().map_or(1, |entry| entry.key + 1);
            for session in &sessions {
                for event in outbox::unqueued_events(&entries, session) {
                    let entry = Entry {
                        key: next_key,
                        job: Job::CookEvent {
                            session: session.id.clone(),
                            event,
                        },
                    };
                    idb::put(idb::OUTBOX, entry.key as f64, &entry);
                    entries.push(entry);
                    next_key += 1;
                }
            }
            self.next_key.set_value(next_key);
            self.recipes.set(
                records
                    .into_iter()
                    .map(|rec| (rec.id.clone(), Arc::new(Recipe::new(rec))))
                    .collect(),
            );
            self.sessions
                .set(sessions.into_iter().map(|s| (s.id.clone(), s)).collect());
            self.pinned.set(pinned);
            self.outbox.set(entries);
            self.tags.set(tags);
            self.scales.set(scales);
            self.inbox.set(inbox);
            self.requests.set(requests);
            self.inbox_pending.set(meta.inbox_pending);
            self.sort
                .set(Sort::parse(meta.sort.as_deref().unwrap_or_default()));
            self.has_library.set(meta.seq.is_some());
            self.meta.set_value(meta);
            self.ready.set(true);
            self.connect_events();
            self.sync().await;
        });

        let window = window();
        listen(&window, "online", move |_| self.wake(true));
        // A window that gets the focus back without having been hidden
        // (another window was in front) may have missed changes too.
        listen(&window, "focus", move |_| self.wake(false));
        listen(&window, "offline", move |_| self.online.set(false));
        listen(&window, "resize", move |_| {
            let width = window_width();
            if (self.width.get_untracked() - width).abs() >= 1.0 {
                self.width.set(width);
            }
        });
        listen(
            &crate::platform::document(),
            "visibilitychange",
            move |_| {
                if visible() {
                    self.wake(false);
                    device::check_for_update();
                }
            },
        );
        // The clock only drives countdowns; the handle lives as long as the page.
        // Four times a second, so a countdown changes within a quarter second
        // of its second and never skips one when the interval runs late.
        let _ = set_interval_with_handle(move || self.tick(), Duration::from_millis(250));
    }

    /// The app came back (to the foreground or to the network). Coming to
    /// the front fires several of the events that lead here at once, and one
    /// sync answers them all; the network coming back (`reconnected`) always
    /// syncs, since the sync before it may have failed.
    fn wake(self, reconnected: bool) {
        let now = now_ms();
        if !reconnected && now - self.last_wake.get_value() < 1000 {
            return;
        }
        self.last_wake.set_value(now);
        self.now.set(now);
        let closed = EVENTS.with_borrow(|slot| {
            slot.as_ref()
                .is_none_or(|events| events.ready_state() == web_sys::EventSource::CLOSED)
        });
        if closed {
            self.connect_events();
        }
        spawn_local(async move { self.sync().await });
    }

    fn tick(self) {
        let now = now_ms();
        self.now.set(now);
        let mut due = Vec::new();
        self.sessions.with_untracked(|sessions| {
            for session in sessions.values().filter(|s| session::is_open(s)) {
                for (step, ends_at) in session::project(session).timers {
                    if ends_at <= now {
                        due.push(Alert {
                            session: session.id.clone(),
                            step,
                            ends_at,
                        });
                    }
                }
            }
        });
        let known = self.alerts.get_untracked();
        let new = due.iter().any(|alert| !known.contains(alert));
        // Three tones are easy to miss over a pan: they repeat until the
        // timer is stopped, for its first minute.
        let again = now - self.last_ring.get_value() >= RING_EVERY_MS
            && due.iter().any(|alert| now - alert.ends_at < RING_FOR_MS);
        if (new || again) && visible() {
            self.last_ring.set_value(now);
            device::ring();
        }
        if due != known {
            self.alerts.set(due);
        }
    }

    /// Has the service worker keep every cover thumbnail, so the library
    /// has its pictures offline. A page the worker does not control yet
    /// (the very first launch) asks again once it does.
    pub fn cache_thumbnails(self) {
        let thumbs: Vec<String> = self.recipes.with_untracked(|recipes| {
            recipes
                .values()
                .filter_map(|recipe| cover(&recipe.rec))
                .map(|image| paths::image(&image.digest, ImageVariant::Thumb))
                .collect()
        });
        device::cache_images(&thumbs);
    }

    /// Has the service worker drop cached `/img/` files no recipe refers to
    /// any more, so the image cache cannot grow without bound.
    fn prune_images(self) {
        let digests: Vec<String> = self.recipes.with_untracked(|recipes| {
            recipes
                .values()
                .flat_map(|recipe| recipe.rec.images.iter().map(|image| image.digest.clone()))
                .collect()
        });
        device::prune_images(&digests);
    }

    // --- Reading ---------------------------------------------------------

    pub fn recipe(self, id: &str) -> Option<Arc<Recipe>> {
        self.recipes.with(|recipes| recipes.get(id).cloned())
    }

    pub fn scale(self, id: &str) -> f64 {
        self.scales
            .with(|scales| scales.get(id).copied())
            .unwrap_or(1.0)
    }

    /// The document a session cooks from and whether the recipe has moved on
    /// since: the session's pinned document (sent by the server, or stored
    /// with a session started on this phone) while it matches the session's
    /// revision, else the recipe's head.
    pub fn session_doc(
        self,
        session: &CookSession,
        state: &Projection,
    ) -> Option<(Arc<RecipeDoc>, bool)> {
        let recipe = self.recipe(&session.recipe_id);
        let head_revision = recipe.as_ref().map(|recipe| recipe.rec.revision);
        let outdated = head_revision.is_some_and(|head| head > state.revision);
        let pinned = self
            .pinned
            .with(|pinned| pinned.get(&session.id).cloned())
            .filter(|(revision, _)| *revision == state.revision)
            .map(|(_, doc)| doc);
        pinned
            .or_else(|| recipe.and_then(|recipe| recipe.doc.clone()))
            .map(|doc| (shown_variant(doc, state.variant.as_deref()), outdated))
    }

    pub fn show_toast(self, text: impl Into<String>, undo: Option<Undo>) {
        let lasts = if undo.is_some() {
            UNDO_WINDOW
        } else {
            Duration::from_secs(3)
        };
        self.show_toast_for(text, undo, lasts);
    }

    /// A toast that leaves after `lasts`: a short one confirms something the
    /// screen already shows.
    pub fn show_toast_for(self, text: impl Into<String>, undo: Option<Undo>, lasts: Duration) {
        let id = self.toast_seq.get_value() + 1;
        self.toast_seq.set_value(id);
        self.toast.set(Some(Toast {
            id,
            text: text.into(),
            undo,
        }));
        set_timeout(
            move || {
                if self
                    .toast
                    .with_untracked(|toast| toast.as_ref().is_some_and(|t| t.id == id))
                {
                    self.toast.set(None);
                }
            },
            lasts,
        );
    }

    /// Offers "`text` · Reload" and returns the offer's id.
    pub fn offer_reload(self, text: &'static str, reload: Undo) -> u64 {
        let id = self.stale_seq.get_value() + 1;
        self.stale_seq.set_value(id);
        self.stale.set(Some(Stale { id, text, reload }));
        id
    }

    /// Takes offer `id` back, unless another screen has made its own since.
    pub fn withdraw_reload(self, id: u64) {
        if self
            .stale
            .with_untracked(|stale| stale.as_ref().is_some_and(|stale| stale.id == id))
        {
            self.stale.set(None);
        }
    }

    // --- Local writes ----------------------------------------------------

    fn put_recipe(self, rec: RecipeRecord) {
        idb::put(idb::RECIPES, rec.id.as_str(), &rec);
        self.recipes.update(|recipes| {
            recipes.insert(rec.id.clone(), Arc::new(Recipe::new(rec)));
        });
    }

    fn put_session(self, session: CookSession) {
        // Decoding is skipped while the held copy is of the same revision. A
        // rebase made here and not yet sent keeps its own, newer copy.
        let held = self
            .pinned
            .with_untracked(|pinned| pinned.get(&session.id).map(|(revision, _)| *revision));
        if held != Some(session.revision)
            && session::project(&session).revision == session.revision
            && let Some(doc) = decode_pinned(&session)
        {
            self.pinned.update(|pinned| {
                pinned.insert(session.id.clone(), (session.revision, doc));
            });
        }
        idb::put(idb::SESSIONS, session.id.as_str(), &session);
        self.sessions.update(|sessions| {
            sessions.insert(session.id.clone(), session);
        });
    }

    fn drop_session(self, id: &str) {
        idb::delete(idb::SESSIONS, id);
        self.sessions.update(|sessions| {
            sessions.remove(id);
        });
        self.pinned.update(|pinned| {
            pinned.remove(id);
        });
    }

    fn save_meta(self) {
        idb::put(idb::KV, "meta", &self.meta.get_value());
    }

    fn take_key(self) -> u64 {
        let key = self.next_key.get_value();
        self.next_key.set_value(key + 1);
        key
    }

    fn enqueue(self, job: Job) {
        let entry = Entry {
            key: self.take_key(),
            job,
        };
        idb::put(idb::OUTBOX, entry.key as f64, &entry);
        self.outbox.update(|entries| entries.push(entry));
        spawn_local(async move { self.pump().await });
    }

    pub fn set_scale(self, recipe: &str, factor: f64) {
        self.scales.update(|scales| {
            scales.insert(recipe.to_owned(), factor);
        });
        idb::put(idb::KV, "scales", &self.scales.get_untracked());
    }

    pub fn set_sort(self, sort: Sort) {
        self.sort.set(sort);
        self.meta
            .update_value(|meta| meta.sort = Some(sort.as_str().to_owned()));
        self.save_meta();
    }

    /// Saves the notes field. The text is in the outbox at once; sending
    /// waits for a pause in typing.
    pub fn set_notes(self, recipe: &str, text: String) {
        let Some(current) = self
            .recipes
            .with_untracked(|recipes| recipes.get(recipe).cloned())
        else {
            return;
        };
        if current.rec.notes == text {
            return;
        }
        let op = ClientOp {
            id: random_id("op_", 12),
            at: now_ms(),
            kind: ClientOpKind::Notes {
                recipe_id: recipe.to_owned(),
                text: text.clone(),
                base_rev: current.rec.notes_rev,
            },
        };
        let mut rec = current.rec.clone();
        rec.notes = text;
        idb::atomic(|| {
            self.put_recipe(rec);
            let key = self.take_key();
            let in_flight = self.in_flight.get_value();
            self.outbox
                .update(|entries| outbox::enqueue_notes(entries, &in_flight, key, op));
            if let Some(last) = self
                .outbox
                .with_untracked(|entries| entries.last().cloned())
            {
                idb::put(idb::OUTBOX, last.key as f64, &last);
            }
        });
        if let Some(timer) = self.notes_timer.get_value() {
            timer.clear();
        }
        let timer = set_timeout_with_handle(
            move || spawn_local(async move { self.pump().await }),
            NOTES_DEBOUNCE,
        );
        self.notes_timer.set_value(timer.ok());
    }

    /// Sends a pending notes edit now (the field lost focus).
    pub fn flush(self) {
        if let Some(timer) = self.notes_timer.get_value() {
            timer.clear();
            self.notes_timer.set_value(None);
        }
        spawn_local(async move { self.pump().await });
    }

    /// Changes one ingredient's amount in the recipe itself. The new revision
    /// is on screen at once and the server writes the same change from the
    /// outbox. Open cook sessions that show the revision it replaces, and the
    /// session it was made from (`from`), move to the new one, so cook mode
    /// shows the amount at once. Returns the amount it replaced, for an undo.
    pub fn change_amount(
        self,
        recipe: &str,
        ingredient: &str,
        qty: f64,
        unit: Option<String>,
        from: Option<&str>,
    ) -> Option<(f64, Option<String>)> {
        let current = self
            .recipes
            .with_untracked(|recipes| recipes.get(recipe).cloned())?;
        let before = current.doc.as_ref().and_then(|doc| {
            omni_recipe_format::Index::build(doc)
                .ingredient(ingredient)
                .and_then(|found| found.qty.map(|qty| (qty, found.unit.clone())))
        })?;
        let changed = outbox::with_amount(&current.rec, ingredient, qty, unit.as_deref())?;
        let revision = changed.revision;
        let following: Vec<String> = self.sessions.with_untracked(|sessions| {
            sessions
                .values()
                .filter(|session| session::follows(session, recipe, current.rec.revision, from))
                .map(|session| session.id.clone())
                .collect()
        });
        idb::atomic(|| {
            self.put_recipe(changed);
            self.enqueue(Job::Op {
                op: ClientOp {
                    id: random_id("op_", 12),
                    at: now_ms(),
                    kind: ClientOpKind::Amount {
                        recipe_id: recipe.to_owned(),
                        ingredient_id: ingredient.to_owned(),
                        qty,
                        unit,
                        base_revision: current.rec.revision,
                    },
                },
            });
            // Queued behind the amount, so the server has the revision
            // before it is asked to move a session to it.
            for session in &following {
                self.cook_event(session, CookEventKind::Rebase { revision });
            }
        });
        Some(before)
    }

    /// The pinned recipes the library shows, first to last.
    pub fn pins(self) -> Vec<String> {
        self.recipes
            .with(|recipes| library::pinned(recipes.values().map(|recipe| &recipe.item)))
    }

    /// Sets the pinned recipes, in order. Archived recipes keep their pin
    /// behind the ones listed, as the server holds it.
    pub fn set_pins(self, ids: Vec<String>) {
        let records: Vec<RecipeRecord> = self
            .recipes
            .with_untracked(|recipes| recipes.values().map(|recipe| recipe.rec.clone()).collect());
        let mut hidden: Vec<(u32, String)> = records
            .iter()
            .filter(|rec| rec.state == RecipeState::Archived && !ids.contains(&rec.id))
            .filter_map(|rec| Some((rec.pinned?, rec.id.clone())))
            .collect();
        hidden.sort();
        let mut all = ids;
        all.extend(hidden.into_iter().map(|(_, id)| id));
        idb::atomic(|| {
            for mut rec in records {
                let place = all
                    .iter()
                    .position(|id| *id == rec.id)
                    .and_then(|place| u32::try_from(place).ok());
                if rec.pinned != place {
                    rec.pinned = place;
                    self.put_recipe(rec);
                }
            }
            self.enqueue(Job::Op {
                op: ClientOp {
                    id: random_id("op_", 12),
                    at: now_ms(),
                    kind: ClientOpKind::Pins { ids: all },
                },
            });
        });
    }

    /// Pins a recipe behind the ones already pinned, or unpins it.
    pub fn pin(self, recipe: &str, pinned: bool) {
        let mut ids: Vec<String> = self
            .recipes
            .with_untracked(|recipes| library::pinned(recipes.values().map(|recipe| &recipe.item)));
        ids.retain(|id| id != recipe);
        if pinned {
            ids.push(recipe.to_owned());
        }
        self.set_pins(ids);
    }

    /// Archives, deletes or brings back. A deleted recipe stays in memory,
    /// hidden, until the next sync drops it, so an undo can show it again.
    pub fn set_state(self, recipe: &RecipeRecord, state: RecipeState) {
        idb::atomic(|| {
            let mut rec = recipe.clone();
            rec.state = state;
            self.put_recipe(rec);
            self.enqueue(Job::Op {
                op: ClientOp {
                    id: random_id("op_", 12),
                    at: now_ms(),
                    kind: ClientOpKind::State {
                        recipe_id: recipe.id.clone(),
                        state,
                    },
                },
            });
        })
    }

    /// Stores a request for the kitchen agent. It is in the outbox at once,
    /// so it is sent when the server can be reached, however long that takes.
    pub fn ask(self, text: &str, ask: Ask) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        self.enqueue(Job::Op {
            op: ClientOp {
                id: random_id("op_", 12),
                at: now_ms(),
                kind: ClientOpKind::Request {
                    request_id: random_id("req_", 8),
                    text: text.to_owned(),
                    origin: ask.origin,
                    recipe_id: ask.recipe_id,
                    revision: ask.revision,
                    scale: ask.scale,
                    step: ask.step,
                },
            },
        });
        self.show_toast(
            if self.online.get_untracked() {
                "Sent to Kitchen"
            } else {
                "Saved. Kitchen gets it when Fond is back online."
            },
            None,
        );
    }

    /// Starts a session on the phone; the server learns of it from the outbox.
    pub fn start_cook(self, recipe: &Recipe, scale: f64, variant: Option<String>) -> String {
        let id = idb::atomic(|| {
            let now = now_ms();
            let id = random_id("cook_", 8);
            let request = StartCookRequest {
                id: id.clone(),
                recipe_id: recipe.rec.id.clone(),
                revision: recipe.rec.revision,
                scale,
                at: now,
            };
            self.put_session(CookSession {
                id: id.clone(),
                recipe_id: recipe.rec.id.clone(),
                revision: recipe.rec.revision,
                scale,
                state: CookState::Active,
                started_at: now,
                finished_at: None,
                last_event_at: now,
                events: Vec::new(),
                doc: Some(recipe.rec.doc.clone()),
            });
            self.enqueue(Job::StartCook { request });
            id
        });
        if variant.is_some() {
            self.cook_event(&id, CookEventKind::Variant { variant });
        }
        id
    }

    /// Appends an event to a session: applied locally now, sent in order.
    pub fn cook_event(self, session_id: &str, kind: CookEventKind) {
        idb::atomic(|| {
            let now = now_ms();
            let event = session::app_event(random_id("ev_", 12), now, kind);
            let known = self.sessions.with_untracked(|s| s.get(session_id).cloned());
            if let Some(mut session) = known {
                if let CookEventKind::Rebase { revision } = &event.kind
                    && let Some(recipe) = self
                        .recipes
                        .with_untracked(|recipes| recipes.get(&session.recipe_id).cloned())
                    && recipe.rec.revision == *revision
                {
                    session.revision = *revision;
                    session.doc = Some(recipe.rec.doc.clone());
                }
                if let CookEventKind::Finish { .. } = &event.kind {
                    self.finished_locally(&session, now);
                }
                session.events.push(event.clone());
                session.last_event_at = now;
                self.put_session(session);
            }
            self.enqueue(Job::CookEvent {
                session: session_id.to_owned(),
                event,
            });
        })
    }

    /// Shows the cook in the library before the server confirms it.
    fn finished_locally(self, session: &CookSession, at: Ms) {
        let recipe = self
            .recipes
            .with_untracked(|recipes| recipes.get(&session.recipe_id).cloned());
        if let Some(recipe) = recipe {
            let mut rec = recipe.rec.clone();
            rec.cook_count += 1;
            rec.last_cooked_at = Some(at);
            self.put_recipe(rec);
        }
    }

    /// Queues a photo of the finished dish for a session.
    pub async fn add_cook_photo(self, session: &str, file: &web_sys::File) -> bool {
        let Ok(bytes) = device::downscale_jpeg(file, PHOTO_EDGE).await else {
            return false;
        };
        let blob = random_id("blob_", 12);
        if !idb::put_bytes(&blob, &bytes).await {
            return false;
        }
        self.enqueue(Job::CookPhoto {
            session: session.to_owned(),
            blob,
        });
        true
    }

    /// Shrinks the chosen photos and queues them as one inbox item.
    pub async fn add_inbox_photos(self, files: Vec<web_sys::File>) {
        let total = files.len();
        let mut blobs = Vec::new();
        for (done, file) in files.iter().enumerate() {
            self.preparing.set(Some((done, total)));
            let Ok(bytes) = device::downscale_jpeg(file, PHOTO_EDGE).await else {
                continue;
            };
            let blob = random_id("blob_", 12);
            if idb::put_bytes(&blob, &bytes).await {
                blobs.push(blob);
            }
        }
        self.preparing.set(None);
        if blobs.len() < total {
            let lost = total - blobs.len();
            self.show_toast(
                format!(
                    "{lost} photo{} could not be read",
                    if lost == 1 { "" } else { "s" }
                ),
                None,
            );
        }
        let item = random_id("inb_", 10);
        let count = u32::try_from(blobs.len()).unwrap_or(u32::MAX);
        for (index, blob) in (0u32..).zip(blobs) {
            self.enqueue(Job::InboxPhoto {
                item: item.clone(),
                index,
                count,
                title: None,
                blob,
            });
        }
    }

    // --- The outbox ------------------------------------------------------

    fn done(self, keys: &[u64]) {
        for key in keys {
            idb::delete(idb::OUTBOX, *key as f64);
        }
        self.outbox
            .update(|entries| entries.retain(|entry| !keys.contains(&entry.key)));
    }

    /// Sends what is queued, in order, until the queue is empty or the
    /// server cannot be reached; then retries with backoff.
    pub async fn pump(self) {
        if self.pumping.get_value() {
            return;
        }
        self.pumping.set_value(true);
        while let Some(batch) = self
            .outbox
            .with_untracked(|entries| outbox::next_batch(entries))
        {
            let keys = match &batch {
                Batch::Ops { keys, .. } | Batch::Events { keys, .. } => keys.clone(),
                Batch::Start { key, .. }
                | Batch::CookPhoto { key, .. }
                | Batch::InboxPhoto { key, .. } => {
                    vec![*key]
                }
            };
            let refused = match &batch {
                Batch::Events {
                    session, events, ..
                } => Some((
                    session.clone(),
                    events
                        .iter()
                        .map(|e| e.client_event_id.clone())
                        .collect::<Vec<_>>(),
                )),
                _ => None,
            };
            self.in_flight.set_value(keys.clone());
            let status = self.send(batch).await;
            self.in_flight.set_value(Vec::new());
            let outcome = outbox::classify(status);
            // Events the server will never take leave the session too, or
            // the screen would keep showing what was not recorded.
            if outcome == Outcome::Drop
                && let Some((session_id, ids)) = refused
                && let Some(mut session) = self
                    .sessions
                    .with_untracked(|s| s.get(&session_id).cloned())
            {
                session
                    .events
                    .retain(|event| !ids.contains(&event.client_event_id));
                self.put_session(session);
            }
            match outcome {
                Outcome::Sent | Outcome::Drop => {
                    self.done(&keys);
                    self.failures.set_value(0);
                    self.online.set(true);
                }
                Outcome::Retry => {
                    if status.is_none() {
                        self.online.set(false);
                    }
                    let failures = self.failures.get_value();
                    self.failures.set_value(failures + 1);
                    set_timeout(
                        move || spawn_local(async move { self.pump().await }),
                        Duration::from_millis(u64::from(outbox::backoff_ms(failures))),
                    );
                    break;
                }
            }
        }
        self.pumping.set_value(false);
    }

    /// Sends one batch and applies the answer; the HTTP status, or none when
    /// no answer came.
    async fn send(self, batch: Batch) -> Option<u16> {
        fn status<T>(result: &Result<T, NetError>) -> Option<u16> {
            match result {
                Ok(_) => Some(200),
                Err(error) => error.status(),
            }
        }
        match batch {
            Batch::Ops { request, .. } => {
                let result = net::post_json::<_, OpsResponse>(paths::OPS, &request).await;
                if let Ok(response) = &result {
                    let mut asked = false;
                    for (op, answer) in request.ops.iter().zip(&response.results) {
                        if matches!(op.kind, ClientOpKind::Request { .. }) {
                            asked = true;
                            if !answer.ok {
                                self.show_toast(
                                    format!(
                                        "The request was not sent: {}",
                                        answer.error.as_deref().unwrap_or("the server refused it")
                                    ),
                                    None,
                                );
                            }
                            continue;
                        }
                        // A refused amount comes back with the recipe as it
                        // is, which replaces what the app showed ahead of it.
                        let refused_amount =
                            !answer.ok && matches!(op.kind, ClientOpKind::Amount { .. });
                        if refused_amount {
                            self.show_toast(
                                format!(
                                    "The amount was not changed: {}",
                                    answer.error.as_deref().unwrap_or("the server refused it")
                                ),
                                None,
                            );
                        }
                        let Some(stored) = answer
                            .recipe
                            .clone()
                            .filter(|_| answer.ok || refused_amount)
                        else {
                            continue;
                        };
                        if let ClientOpKind::Notes {
                            recipe_id, text, ..
                        } = &op.kind
                        {
                            self.outbox.update(|entries| {
                                outbox::rebase_notes(entries, recipe_id, text, &stored);
                            });
                            for entry in self.outbox.get_untracked() {
                                idb::put(idb::OUTBOX, entry.key as f64, &entry);
                            }
                        }
                        // This batch is still queued; only what is behind it counts as unsent.
                        let behind: Vec<Entry> = self.outbox.with_untracked(|entries| {
                            entries
                                .iter()
                                .filter(|e| !self.in_flight.get_value().contains(&e.key))
                                .cloned()
                                .collect()
                        });
                        self.put_recipe(outbox::overlay_pending(stored, &behind));
                    }
                    if asked {
                        spawn_local(async move { self.load_requests().await });
                    }
                }
                status(&result)
            }
            Batch::Start { request, .. } => {
                let result = net::post_json::<_, CookSession>(paths::COOK, &request).await;
                match &result {
                    Ok(session) => self.merge_session(session.clone()),
                    Err(NetError::Status(status @ 400..=499))
                        if outbox::classify(Some(*status)) == Outcome::Drop =>
                    {
                        // Its events would be answered 425 for ever.
                        let mut gone = Vec::new();
                        self.outbox.update(|entries| {
                            gone = outbox::drop_session(entries, &request.id);
                        });
                        for entry in gone {
                            idb::delete(idb::OUTBOX, entry.key as f64);
                            if let Job::CookPhoto { blob, .. } = entry.job {
                                idb::delete(idb::BLOBS, blob);
                            }
                        }
                        self.drop_session(&request.id);
                    }
                    Err(_) => {}
                }
                status(&result)
            }
            Batch::Events {
                session, events, ..
            } => {
                let request = AppendCookEventsRequest { events };
                let result =
                    net::post_json::<_, CookSession>(&paths::cook_events(&session), &request).await;
                if let Ok(stored) = &result {
                    self.merge_session(stored.clone());
                }
                status(&result)
            }
            Batch::CookPhoto { session, blob, .. } => {
                // The blob's name is this upload's id: random, stored with the
                // entry, and so the same on every retry.
                self.upload(&paths::cook_photo(&session, &blob), &blob)
                    .await
            }
            Batch::InboxPhoto {
                item,
                index,
                count,
                title,
                blob,
                ..
            } => {
                let path = paths::inbox_upload(&item, index, count, title.as_deref());
                let status = self.upload(&path, &blob).await;
                if index + 1 == count && matches!(status, Some(200..=299)) {
                    spawn_local(async move { self.load_inbox().await });
                }
                status
            }
        }
    }

    async fn upload(self, path: &str, blob: &str) -> Option<u16> {
        // Bytes that are gone (storage was cleared) can never be sent.
        let Some(bytes) = idb::get_bytes(blob).await else {
            return Some(410);
        };
        let result = net::post_jpeg(path, &bytes).await;
        let status = match result {
            Ok(_) => Some(200),
            Err(error) => error.status(),
        };
        if outbox::classify(status) != Outcome::Retry {
            idb::delete(idb::BLOBS, blob);
        }
        status
    }

    // --- Sync ------------------------------------------------------------

    /// Stores the server's copy of a session under the events not yet sent.
    fn merge_session(self, server: CookSession) {
        let pending = self
            .outbox
            .with_untracked(|entries| outbox::pending_event_ids(entries));
        let in_flight = self.in_flight.get_value();
        let waiting = self.outbox.with_untracked(|entries| {
            let behind: Vec<Entry> = entries
                .iter()
                .filter(|entry| !in_flight.contains(&entry.key))
                .cloned()
                .collect();
            outbox::session_pending(&behind, &server.id)
        });
        if server.state != CookState::Active && !waiting {
            self.drop_session(&server.id);
            return;
        }
        let local = self.sessions.with_untracked(|s| s.get(&server.id).cloned());
        self.put_session(session::merge_server(server, local.as_ref(), &pending));
    }

    pub async fn refresh_session(self, id: String) {
        match net::get_json::<CookSession>(&paths::cook(&id)).await {
            Ok(session) => self.merge_session(session),
            Err(NetError::Status(404)) => {
                let waiting = self
                    .outbox
                    .with_untracked(|entries| outbox::session_pending(entries, &id));
                if !waiting {
                    self.drop_session(&id);
                }
            }
            Err(_) => {}
        }
    }

    /// Brings the phone's copy up to date. Unsent writes go first so the
    /// answer already includes them.
    pub async fn sync(self) {
        if self.syncing.get_value() {
            self.sync_again.set_value(true);
            return;
        }
        self.syncing.set_value(true);
        loop {
            self.sync_again.set_value(false);
            self.pump().await;
            let since = self.meta.with_value(|meta| meta.seq);
            if let Some(response) = self.fetch_sync(since).await {
                let complete = self.apply_sync(response);
                if !complete && let Some(full) = self.fetch_sync(None).await {
                    self.apply_sync(full);
                }
                self.load_requests().await;
            }
            if !self.sync_again.get_value() {
                break;
            }
        }
        self.syncing.set_value(false);
    }

    async fn fetch_sync(self, since: Option<u64>) -> Option<SyncResponse> {
        match net::get_json::<SyncResponse>(&paths::sync(since)).await {
            Ok(response) => {
                self.online.set(true);
                Some(response)
            }
            Err(error) => {
                if error == NetError::Offline {
                    self.online.set(false);
                }
                None
            }
        }
    }

    /// Applies a sync answer; false when the phone's recipe count then
    /// disagrees with the server's and a full sync is needed.
    fn apply_sync(self, response: SyncResponse) -> bool {
        let entries = self.outbox.get_untracked();
        let keeps = |id: &str| {
            matches!(
                outbox::pending_state(&entries, id),
                Some(RecipeState::Active | RecipeState::Archived)
            )
        };
        let mut recipes = self.recipes.get_untracked();
        let mut deletes = Vec::new();
        if response.full {
            let listed: Vec<&str> = response.recipes.iter().map(|rec| rec.id.as_str()).collect();
            deletes = recipes
                .keys()
                .filter(|id| !listed.contains(&id.as_str()) && !keeps(id))
                .cloned()
                .collect();
        } else {
            deletes.extend(response.removed.iter().filter(|id| !keeps(id)).cloned());
        }
        for id in &deletes {
            recipes.remove(id);
        }
        let mut puts = Vec::new();
        for rec in response.recipes {
            let rec = outbox::overlay_pending(rec, &entries);
            puts.push((rec.id.clone(), rec.clone()));
            recipes.insert(rec.id.clone(), Arc::new(Recipe::new(rec)));
        }
        let live = recipes
            .values()
            .filter(|recipe| recipe.rec.state != RecipeState::Trashed && !keeps(&recipe.rec.id))
            .count();
        let complete = response.full || u32::try_from(live).ok() == Some(response.live_count);
        idb::write_all(idb::RECIPES, puts, deletes, false);

        self.recipes.set(recipes);
        self.cache_thumbnails();
        self.prune_images();

        idb::put(idb::KV, "tags", &response.tags);
        self.tags.set(response.tags);
        self.inbox_pending.set(response.inbox_pending);

        let listed: Vec<String> = response.sessions.iter().map(|s| s.id.clone()).collect();
        for session in response.sessions {
            self.merge_session(session);
        }
        let stale: Vec<String> = self.sessions.with_untracked(|sessions| {
            sessions
                .keys()
                .filter(|id| !listed.contains(id) && !outbox::session_pending(&entries, id))
                .cloned()
                .collect()
        });
        for id in stale {
            self.drop_session(&id);
        }

        let new_build = self.meta.with_value(|meta| {
            response.build.is_some() && meta.build.is_some() && meta.build != response.build
        });
        self.meta.update_value(|meta| {
            meta.seq = Some(response.seq);
            meta.inbox_pending = response.inbox_pending;
            if response.build.is_some() {
                meta.build = response.build;
            }
        });
        self.save_meta();
        self.has_library.set(true);
        if new_build {
            device::check_for_update();
        }
        complete
    }

    fn connect_events(self) {
        let Ok(events) = web_sys::EventSource::new(paths::EVENTS) else {
            return;
        };
        let data = |event: web_sys::Event| {
            event
                .dyn_into::<web_sys::MessageEvent>()
                .ok()
                .and_then(|message| message.data().as_string())
                .unwrap_or_default()
        };
        listen(&events, "open", move |_| {
            self.online.set(true);
            spawn_local(async move { self.pump().await });
        });
        listen(&events, "error", move |_| self.online.set(false));
        listen(&events, sse::HELLO, move |event| {
            let Ok(hello) = serde_json::from_str::<Hello>(&data(event)) else {
                return;
            };
            let build_changed = self.meta.with_value(|meta| {
                meta.build.is_some() && hello.build.is_some() && meta.build != hello.build
            });
            if build_changed {
                device::check_for_update();
            }
            if self.meta.with_value(|meta| meta.seq) != Some(hello.seq) {
                spawn_local(async move { self.sync().await });
            }
        });
        listen(&events, sse::LIBRARY, move |_| {
            let inbox_open = self.route.get_untracked() == Route::Inbox;
            spawn_local(async move {
                self.sync().await;
                // An agent importing an item changes the list being looked at.
                if inbox_open {
                    self.load_inbox().await;
                }
            });
        });
        listen(&events, sse::SESSION, move |event| {
            if let Ok(changed) = serde_json::from_str::<SessionChanged>(&data(event)) {
                spawn_local(async move { self.refresh_session(changed.id).await });
            }
        });
        listen(&events, sse::REQUESTS, move |_| {
            spawn_local(async move { self.load_requests().await });
        });
        EVENTS.with_borrow_mut(|slot| {
            if let Some(old) = slot.replace(events) {
                old.close();
            }
        });
    }

    pub async fn load_inbox(self) {
        if let Ok(list) = net::get_json::<InboxList>(paths::INBOX).await {
            idb::put(idb::KV, "inbox", &list.items);
            self.inbox.set(list.items);
        }
    }

    pub async fn load_requests(self) {
        let asked = self.requests_asked.get_value() + 1;
        self.requests_asked.set_value(asked);
        if let Ok(list) = net::get_json::<RequestList>(paths::REQUESTS).await
            // Two fetches can answer out of order; only the newest counts.
            && self.requests_asked.get_value() == asked
            // An unchanged list leaves the signal alone, so nothing redraws.
            && self.requests.with_untracked(|held| *held != list.items)
        {
            idb::put(idb::KV, "requests", &list.items);
            self.requests.set(list.items);
        }
    }

    /// The stored source of a recipe: fresh when reachable, else the copy
    /// kept from the last time it was viewed.
    pub async fn original(self, recipe: String) -> Option<Original> {
        let key = format!("orig:{recipe}");
        match net::get_json::<Original>(&paths::original(&recipe)).await {
            Ok(original) => {
                idb::put(idb::KV, key, &original);
                Some(original)
            }
            Err(_) => idb::get(idb::KV, &key).await,
        }
    }
}

fn window_width() -> f64 {
    window()
        .inner_width()
        .ok()
        .and_then(|width| width.as_f64())
        .unwrap_or(390.0)
}

/// `doc` as its variant `id` makes it; as written without one, or when the
/// revision no longer has it.
pub fn shown_variant(doc: Arc<RecipeDoc>, id: Option<&str>) -> Arc<RecipeDoc> {
    match id.map(|id| omni_recipe_format::variant_doc(&doc, id)) {
        Some(Ok(made)) => Arc::new(made),
        _ => doc,
    }
}

/// The document stored with a session, decoded.
fn decode_pinned(session: &CookSession) -> Option<Arc<RecipeDoc>> {
    let doc = session.doc.clone()?;
    omni_recipe_format::upgrade(omni_recipe_format::FORMAT, doc)
        .ok()
        .map(Arc::new)
}

impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}
