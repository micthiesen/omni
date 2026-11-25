//! The iCloud IMAP transport: IDLE push on
//! INBOX plus per-folder UID-cursor delta fetch, a 5-minute sweep event,
//! jittered reconnect backoff, bounded read caches, and every mailbox workflow
//! serialized behind one operation permit (selection is connection-global).
//!
//! iCloud quirks: pre-login CAPABILITY is minimal, so capabilities are re-read
//! after LOGIN; parameterized `SELECT (CONDSTORE)` is rejected, so sync is
//! plain UID based (no CONDSTORE/QRESYNC).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::FutureExt as _;
use futures::future::{BoxFuture, Shared, WeakShared};
use omni_core::clock::SharedClock;
use omni_core::email::{EmailAttachment, FetchedEmail};
use omni_core::mail_source::{EmailPoll, MailSource, PollError};
use omni_http::SideEffectMode;
use omni_runtime::ports::{
    ArchiveEcho, EmailFolderScope, EmailReader, EmailReaderHealth, EmailSearch, PortError,
};
use omni_store::Store;
use rand::RngExt as _;
use tokio::sync::{Notify, broadcast};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::archive_service::ArchiveMailbox;
use crate::archive_store::{archive_auto_read_protection, is_archive_action_message};
use crate::attachments::{
    MAX_ATTACHMENT_BYTES, MAX_ATTACHMENT_MESSAGE_BYTES, attachment_part_id,
    declared_attachment_mime_type, encode_stable_attachment_id, safe_attachment_filename,
    valid_attachment_message_id, valid_stable_attachment_id,
};
use crate::cursor::{get_folder_cursor, last_dispatched_at, save_folder_cursor};
use crate::map_message::{
    MessageCoords, SharedEnricher, decode_attachment_blob_id, decode_message_id, estimate_bytes,
    map_parsed_message,
};
use crate::mime::parse_message;
use crate::ops::archive::{
    self as archive_ops, ArchiveIdentity, ArchiveLocation, ArchiveMoveResult,
    ArchiveReconcileResult, ArchiveSnapshot, ArchiveSourceRequest, DeletedSourceState,
};
use crate::ops::auto_read::{
    AutoReadPlan, AutoReadProtection, ProtectionFn, discover_auto_read_plan,
    mark_recent_unread_read,
};
use crate::ops::drafts::{EmailDraftInput, EmailDraftResult, create_draft};
use crate::ops::sent::{BeforeAppend, SentCopyInput, SentCopyResult, append_sent_copy};
use crate::protocol::connect::{Connector, IMAP_HOST};
use crate::protocol::record::{RecordedImapWrite, RecordedWrites, RecordingClient};
use crate::protocol::{
    FetchQuery, IdleEnd, ImapClient, ImapError, MailboxEvent, SearchCriteria, SourceRange, UidSet,
    fetch_one, selected_validity,
};
use crate::read_cache::BoundedReadCache;
use crate::sync::{FolderState, FolderSyncPlan, plan_folder_sync};

const LOG: &str = "IMAP";

/// Folders whose new mail feeds the pipelines. Archive is watched directly
/// because iCloud server-side rules can file mail there before delivery.
pub const FOLDERS: [&str; 2] = ["INBOX", "Archive"];
/// IDLE only pushes for INBOX; this sweep catches mail filed into Archive and
/// pushes lost across reconnects.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// Re-issue IDLE well before the RFC 2177 29-minute limit.
pub const MAX_IDLE: Duration = Duration::from_secs(13 * 60);
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
/// Pause before re-issuing IDLE after the server refused it.
const IDLE_RETRY_DELAY: Duration = Duration::from_secs(30);
const MAX_BACKOFF: Duration = Duration::from_secs(5 * 60);
/// Cap per folder per pass; the cursor advances only past what was fetched.
pub const MAX_EMAILS_PER_PASS: usize = 200;
/// Older messages are cursor-skipped (bulk import guard on INTERNALDATE).
pub const MAX_EMAIL_AGE_MS: i64 = 7 * 24 * 60 * 60_000;
const PARSED_CACHE_TTL_MS: i64 = 5 * 60_000;
const SEARCH_CACHE_TTL_MS: i64 = 30_000;

pub use omni_core::email::DownloadedAttachment;

struct Caches {
    parsed: BoundedReadCache<FetchedEmail>,
    search: BoundedReadCache<Vec<FetchedEmail>>,
    location: BoundedReadCache<MessageCoords>,
    direct: BoundedReadCache<FetchedEmail>,
}

impl Caches {
    fn new() -> Self {
        Self {
            parsed: BoundedReadCache::new(128, 24 * 1024 * 1024, Some(PARSED_CACHE_TTL_MS)),
            search: BoundedReadCache::new(32, 8 * 1024 * 1024, Some(SEARCH_CACHE_TTL_MS)),
            location: BoundedReadCache::new(256, 128 * 1024, Some(SEARCH_CACHE_TTL_MS)),
            direct: BoundedReadCache::new(128, 24 * 1024 * 1024, Some(SEARCH_CACHE_TTL_MS)),
        }
    }

    fn clear(&mut self) {
        self.parsed.clear();
        self.search.clear();
        self.location.clear();
        self.direct.clear();
    }
}

#[derive(Default)]
struct State {
    stopped: bool,
    reconnect_scheduled: bool,
    runtime: Option<CancellationToken>,
    /// Special-use mailboxes are rediscovered after every new connection.
    auto_read: Option<AutoReadPlan>,
    /// Last bulk-import-guard skip count per folder, to de-noise repeat logs.
    last_skip_counts: HashMap<String, usize>,
}

type ConnectFlight = WeakShared<BoxFuture<'static, Result<(), ImapError>>>;

struct Inner {
    connector: Arc<dyn Connector>,
    store: Store,
    clock: SharedClock,
    enricher: SharedEnricher,
    mode: SideEffectMode,
    tracker: TaskTracker,
    conn: tokio::sync::Mutex<Option<Box<dyn ImapClient>>>,
    waiters: AtomicUsize,
    idle_interrupt: Notify,
    idle_resume: Notify,
    connect_flight: Mutex<Option<ConnectFlight>>,
    caches: Mutex<Caches>,
    events: broadcast::Sender<()>,
    state: Mutex<State>,
    active: AtomicBool,
    recorded: RecordedWrites,
}

/// Everything the transport needs.
pub struct TransportOptions {
    pub connector: Arc<dyn Connector>,
    pub store: Store,
    pub clock: SharedClock,
    pub enricher: SharedEnricher,
    pub mode: SideEffectMode,
    pub tracker: TaskTracker,
}

/// The IMAP transport; cheap to clone.
#[derive(Clone)]
pub struct ImapTransport {
    inner: Arc<Inner>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn err(operation: impl Into<String>, detail: impl Into<String>) -> ImapError {
    ImapError::new(operation, detail)
}

fn wrap(operation: impl Into<String>) -> impl FnOnce(ImapError) -> ImapError {
    let operation = operation.into();
    move |e| ImapError::wrap(operation, e)
}

fn message_cache_key(folder: &str, validity: &str, uid: u32) -> String {
    format!("{folder}\0{validity}\0{uid}")
}

fn iso_ms(value: &str) -> Option<i64> {
    value
        .parse::<jiff::Timestamp>()
        .ok()
        .map(|t| t.as_millisecond())
}

fn search_cache_key(options: &EmailSearch) -> String {
    let trimmed = |v: &Option<String>| v.as_deref().map(str::trim).unwrap_or("").to_owned();
    let folder = match options.folder {
        Some(EmailFolderScope::Inbox) => "inbox",
        Some(EmailFolderScope::Archive) => "archive",
        Some(EmailFolderScope::Sent) => "sent",
        Some(EmailFolderScope::All) | None => "all",
    };
    omni_core::js::json_stringify(&serde_json::json!({
        "query": trimmed(&options.query),
        "from": trimmed(&options.from),
        "to": trimmed(&options.to),
        "subject": trimmed(&options.subject),
        "unread": options.unread,
        "since": options.since_ms.map(omni_core::js::to_iso_string),
        "before": options.before_ms.map(omni_core::js::to_iso_string),
        "folder": folder,
        "limit": options.limit,
    }))
}

fn non_empty(value: &Option<String>) -> Option<String> {
    value.clone().filter(|v| !v.is_empty())
}

/// One serialized mailbox operation. `finish` restores INBOX, applies
/// server events and handles a dead connection; dropping it unfinished only
/// skips the restore (IDLE reselects INBOX before idling).
struct Operation<'t> {
    transport: &'t ImapTransport,
    guard: tokio::sync::MutexGuard<'t, Option<Box<dyn ImapClient>>>,
}

impl Operation<'_> {
    fn client(&mut self) -> Result<&mut dyn ImapClient, ImapError> {
        match self.guard.as_mut() {
            Some(client) if client.usable() => Ok(client.as_mut()),
            _ => Err(err("access connection", "IMAP connection is not available")),
        }
    }

    async fn finish(mut self) {
        if let Some(client) = self.guard.as_mut()
            && client.usable()
            && client.selected().is_none_or(|s| s.path != "INBOX")
            && let Err(error) = client.select("INBOX", true).await
        {
            tracing::debug!(target: LOG, "Failed to reselect INBOX: {error}");
        }
        self.transport.absorb(&mut self.guard);
    }
}

impl Drop for Operation<'_> {
    fn drop(&mut self) {
        let inner = &self.transport.inner;
        if inner.waiters.load(Ordering::SeqCst) == 0 {
            inner.idle_resume.notify_one();
        }
    }
}

impl ImapTransport {
    pub fn new(options: TransportOptions) -> Self {
        let (events, _) = broadcast::channel(16);
        Self {
            inner: Arc::new(Inner {
                connector: options.connector,
                store: options.store,
                clock: options.clock,
                enricher: options.enricher,
                mode: options.mode,
                tracker: options.tracker,
                conn: tokio::sync::Mutex::new(None),
                waiters: AtomicUsize::new(0),
                idle_interrupt: Notify::new(),
                idle_resume: Notify::new(),
                connect_flight: Mutex::new(None),
                caches: Mutex::new(Caches::new()),
                events,
                state: Mutex::new(State::default()),
                active: AtomicBool::new(false),
                recorded: Arc::new(Mutex::new(Vec::new())),
            }),
        }
    }

    /// Short label for logs.
    pub fn name(&self) -> &'static str {
        "IMAP"
    }

    /// True once the first start succeeded.
    pub fn is_active(&self) -> bool {
        self.inner.active.load(Ordering::SeqCst)
    }

    /// Mutations recorded instead of sent in `SideEffectMode::Record`.
    pub fn recorded_writes(&self) -> Vec<RecordedImapWrite> {
        lock(&self.inner.recorded).clone()
    }

    fn now(&self) -> i64 {
        self.inner.clock.now_ms()
    }

    fn emit_mail_event(&self) {
        let _receivers = self.inner.events.send(());
    }

    fn clear_read_caches(&self) {
        lock(&self.inner.caches).clear();
    }

    async fn begin(&self) -> Operation<'_> {
        struct Waiting<'a>(&'a AtomicUsize);
        impl Drop for Waiting<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        let inner = &self.inner;
        inner.waiters.fetch_add(1, Ordering::SeqCst);
        let waiting = Waiting(&inner.waiters);
        inner.idle_interrupt.notify_one();
        let guard = inner.conn.lock().await;
        drop(waiting);
        Operation {
            transport: self,
            guard,
        }
    }

    /// Applies queued server events and handles a dead connection.
    fn absorb(&self, slot: &mut Option<Box<dyn ImapClient>>) {
        let Some(client) = slot.as_mut() else { return };
        let mut mail_event = false;
        for event in client.take_events() {
            match event {
                MailboxEvent::Exists => {
                    lock(&self.inner.caches).search.clear();
                    mail_event = true;
                }
                MailboxEvent::Flags => lock(&self.inner.caches).search.clear(),
                MailboxEvent::Expunge => self.clear_read_caches(),
            }
        }
        if mail_event {
            self.emit_mail_event();
        }
        if !client.usable() {
            *slot = None;
            self.clear_read_caches();
            let stopped = lock(&self.inner.state).stopped;
            if !stopped {
                self.schedule_reconnect();
            }
        }
    }

    /// Installs an already-connected client without the IDLE loop and marks
    /// the transport active (tests drive operations directly).
    #[cfg(feature = "testing")]
    pub async fn attach_client(&self, client: Box<dyn ImapClient>) {
        *self.inner.conn.lock().await = Some(client);
        self.inner.active.store(true, Ordering::SeqCst);
    }

    /// Connects through the single-flight path (tests of connect sharing).
    #[cfg(feature = "testing")]
    pub async fn connect_now(&self) -> Result<(), ImapError> {
        self.connect_single_flight().await
    }

    /// Whether a connection is installed.
    #[cfg(feature = "testing")]
    pub async fn has_client(&self) -> bool {
        self.inner.conn.lock().await.is_some()
    }

    /// Replaces the parsed-message cache bounds.
    #[cfg(feature = "testing")]
    pub fn set_parsed_cache_limits(&self, entries: usize, bytes: usize) {
        lock(&self.inner.caches).parsed =
            BoundedReadCache::new(entries, bytes, Some(PARSED_CACHE_TTL_MS));
    }

    /// Presets the discovered auto-read plan.
    #[cfg(feature = "testing")]
    pub fn set_auto_read_plan(&self, plan: AutoReadPlan) {
        lock(&self.inner.state).auto_read = Some(plan);
    }

    /// Runs `f` with the operation permit held (tests of serialization).
    #[cfg(feature = "testing")]
    pub async fn with_permit<T>(&self, f: impl std::future::Future<Output = T>) -> T {
        let op = self.begin().await;
        let out = f.await;
        drop(op);
        out
    }

    // ---- lifecycle -------------------------------------------------------

    /// Connects (first failure goes to the caller's boot retry), then
    /// supervises reconnects and the sweep until `stop`.
    pub async fn start(&self) -> Result<(), ImapError> {
        let token = CancellationToken::new();
        {
            let mut state = lock(&self.inner.state);
            if let Some(previous) = state.runtime.replace(token.clone()) {
                previous.cancel();
            }
            state.stopped = false;
        }
        if let Err(error) = self.connect_single_flight().await {
            token.cancel();
            let mut state = lock(&self.inner.state);
            if state.runtime.as_ref().is_some_and(|t| t.is_cancelled()) {
                state.runtime = None;
            }
            return Err(error);
        }
        let sweeper = self.clone();
        let sweep_token = token.clone();
        omni_core::spawn::spawn_tracked(&self.inner.tracker, "imap-sweep", async move {
            loop {
                tokio::select! {
                    () = sweep_token.cancelled() => break,
                    () = tokio::time::sleep(SWEEP_INTERVAL) => sweeper.emit_mail_event(),
                }
            }
        });
        self.inner.active.store(true, Ordering::SeqCst);
        self.emit_mail_event();
        Ok(())
    }

    /// Stops supervision, then logs out behind any active mailbox operation.
    pub async fn stop(&self) {
        let runtime = {
            let mut state = lock(&self.inner.state);
            state.stopped = true;
            state.reconnect_scheduled = false;
            state.runtime.take()
        };
        self.clear_read_caches();
        if let Some(token) = runtime {
            token.cancel();
        }
        tracing::info!(target: LOG, "Closing IMAP connection");
        let mut op = self.begin().await;
        if let Some(mut client) = op.guard.take() {
            client.logout().await;
        }
        drop(op);
    }

    /// Connect attempts are shared: concurrent callers await the same attempt.
    async fn connect_single_flight(&self) -> Result<(), ImapError> {
        let flight = {
            let mut slot = lock(&self.inner.connect_flight);
            match slot.as_ref().and_then(WeakShared::upgrade) {
                Some(existing) => existing,
                None => {
                    let transport = self.clone();
                    let fut: BoxFuture<'static, Result<(), ImapError>> =
                        Box::pin(async move { transport.connect().await });
                    let shared: Shared<_> = fut.shared();
                    // Only callers keep the attempt alive: when every caller is
                    // dropped, the attempt (and any half-open client) is dropped.
                    *slot = shared.downgrade();
                    shared
                }
            }
        };
        flight.await
    }

    async fn connect(&self) -> Result<(), ImapError> {
        let mut op = self.begin().await;
        if op.guard.as_ref().is_some_and(|c| c.usable()) {
            drop(op);
            return Ok(());
        }
        let connected = self.inner.connector.connect().await;
        let mut client = connected.map_err(wrap("connect"))?;
        if lock(&self.inner.state).stopped {
            client.logout().await;
            return Err(err("connect", "IMAP transport stopped while connecting"));
        }
        if let Err(error) = client.select("INBOX", true).await {
            client.logout().await;
            return Err(err("select INBOX", error.to_string()));
        }
        let caps = ["IDLE", "CONDSTORE", "QRESYNC", "UIDPLUS", "MOVE"]
            .iter()
            .map(|c| format!("{c}={}", if client.has_capability(c) { "y" } else { "n" }))
            .collect::<Vec<_>>()
            .join(" ");
        tracing::info!(target: LOG, "IMAP connected to {IMAP_HOST} ({caps})");
        let client: Box<dyn ImapClient> = if self.inner.mode == SideEffectMode::Record {
            Box::new(RecordingClient::new(client, self.inner.recorded.clone()))
        } else {
            client
        };
        lock(&self.inner.state).auto_read = None;
        self.clear_read_caches();
        *op.guard = Some(client);
        drop(op);
        let token = lock(&self.inner.state)
            .runtime
            .as_ref()
            .map(CancellationToken::child_token)
            .unwrap_or_default();
        let idler = self.clone();
        omni_core::spawn::spawn_tracked(&self.inner.tracker, "imap-idle", async move {
            idler.idle_loop(token).await;
        });
        Ok(())
    }

    /// Keeps INBOX in IDLE whenever no operation needs the connection.
    async fn idle_loop(&self, cancel: CancellationToken) {
        loop {
            if cancel.is_cancelled() {
                return;
            }
            if self.inner.waiters.load(Ordering::SeqCst) > 0 {
                let resumed = self.inner.idle_resume.notified();
                tokio::select! {
                    () = resumed => {}
                    () = tokio::time::sleep(Duration::from_millis(250)) => {}
                    () = cancel.cancelled() => return,
                }
                continue;
            }
            let mut guard = tokio::select! {
                guard = self.inner.conn.lock() => guard,
                () = cancel.cancelled() => return,
            };
            if self.inner.waiters.load(Ordering::SeqCst) > 0 {
                continue;
            }
            let Some(client) = guard.as_mut() else { return };
            if !client.usable() {
                self.absorb(&mut guard);
                return;
            }
            if client.selected().is_none_or(|s| s.path != "INBOX")
                && let Err(error) = client.select("INBOX", true).await
            {
                tracing::debug!(target: LOG, "Failed to reselect INBOX: {error}");
            }
            let mut failed = false;
            if client.usable() {
                match client.idle(&self.inner.idle_interrupt, MAX_IDLE).await {
                    Ok(IdleEnd::Interrupted | IdleEnd::TimedOut | IdleEnd::ServerEvent) => {}
                    Err(error) => {
                        failed = true;
                        tracing::warn!(target: LOG, "IMAP connection error: {}", error.leaf());
                    }
                }
            }
            let closed = guard.as_ref().is_none_or(|c| !c.usable());
            self.absorb(&mut guard);
            drop(guard);
            if closed {
                return;
            }
            if failed {
                // A refused IDLE on a live session must not spin; operations
                // still get the connection while this waits.
                tokio::select! {
                    () = cancel.cancelled() => return,
                    () = tokio::time::sleep(IDLE_RETRY_DELAY) => {}
                }
            }
        }
    }

    fn schedule_reconnect(&self) {
        let token = {
            let mut state = lock(&self.inner.state);
            if state.reconnect_scheduled || state.stopped {
                return;
            }
            let Some(token) = state.runtime.as_ref().map(CancellationToken::child_token) else {
                return;
            };
            state.reconnect_scheduled = true;
            token
        };
        let transport = self.clone();
        omni_core::spawn::spawn_tracked(&self.inner.tracker, "imap-reconnect", async move {
            transport.reconnect(token).await;
            lock(&transport.inner.state).reconnect_scheduled = false;
        });
    }

    async fn reconnect(&self, cancel: CancellationToken) {
        let jitter_ms: u64 = rand::rng().random_range(0..=3000);
        tracing::warn!(target: LOG, "IMAP connection closed, reconnecting in {jitter_ms}ms");
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(Duration::from_millis(jitter_ms)) => {}
        }
        let mut delay = INITIAL_BACKOFF;
        loop {
            match self.connect_single_flight().await {
                Ok(()) => break,
                Err(error) => {
                    tracing::warn!(target: LOG, "IMAP reconnect failed: {error}");
                    let factor: f64 = rand::rng().random_range(0.8..=1.2);
                    let wait = delay.mul_f64(factor).min(MAX_BACKOFF);
                    tokio::select! {
                        () = cancel.cancelled() => return,
                        () = tokio::time::sleep(wait) => {}
                    }
                    delay = (delay * 2).min(MAX_BACKOFF);
                }
            }
        }
        // Pushes during the gap are gone: treat reconnect as a mail event.
        self.emit_mail_event();
    }

    // ---- polling ---------------------------------------------------------

    /// New mail since the persisted cursors plus the commit advancing them.
    pub async fn poll_new_emails(&self) -> Result<EmailPoll, ImapError> {
        let mut op = self.begin().await;
        let result = async {
            let client = op.client()?;
            let mut emails = Vec::new();
            let mut commits: Vec<(String, String, u32)> = Vec::new();
            for folder in FOLDERS {
                match self.poll_folder(client, folder).await {
                    Ok((found, commit)) => {
                        emails.extend(found);
                        commits.extend(commit);
                    }
                    Err(error) => {
                        tracing::warn!(target: LOG, "IMAP poll failed for folder \"{folder}\": {error}");
                    }
                }
            }
            self.auto_read(client).await;
            Ok::<_, ImapError>((emails, commits))
        }
        .await;
        op.finish().await;
        let (emails, commits) = result?;
        let store = self.inner.store.clone();
        Ok(EmailPoll {
            emails,
            commit: Box::new(move || {
                Box::pin(async move {
                    for (folder, validity, next) in commits {
                        save_folder_cursor(&store, &folder, &validity, next)
                            .await
                            .map_err(|e| PollError::Commit(e.to_string()))?;
                    }
                    Ok(())
                })
            }),
        })
    }

    async fn auto_read(&self, client: &mut dyn ImapClient) {
        let plan = lock(&self.inner.state).auto_read.clone();
        let plan = match plan {
            Some(plan) => plan,
            None => match discover_auto_read_plan(client).await {
                Ok(plan) => {
                    lock(&self.inner.state).auto_read = Some(plan.clone());
                    plan
                }
                Err(error) => {
                    tracing::warn!(target: LOG, "IMAP auto-read mailbox discovery failed: {}", error.leaf());
                    return;
                }
            },
        };
        let store = self.inner.store.clone();
        let archive_folders = plan.archive_folders.clone();
        let protection: Box<ProtectionFn> =
            Box::new(move |folder: String, validity: Option<String>| {
                let store = store.clone();
                let is_archive = archive_folders.contains(&folder);
                Box::pin(async move {
                    if !is_archive {
                        return Ok(AutoReadProtection::default());
                    }
                    archive_auto_read_protection(&store, &folder, validity.as_deref())
                        .await
                        .map_err(|e| e.to_string())
                })
            });
        mark_recent_unread_read(client, &plan.folders, self.now(), Some(protection.as_ref())).await;
    }

    async fn poll_folder(
        &self,
        client: &mut dyn ImapClient,
        folder: &str,
    ) -> Result<(Vec<FetchedEmail>, Option<(String, String, u32)>), ImapError> {
        let status = client
            .status(folder)
            .await
            .map_err(wrap(format!("STATUS {folder}")))?;
        let (Some(uid_next), Some(validity)) = (status.uid_next, status.uid_validity) else {
            return Err(err(
                format!("STATUS {folder}"),
                "STATUS returned no uidNext/uidValidity",
            ));
        };
        let uid_validity = validity.to_string();
        let cursor = get_folder_cursor(&self.inner.store, folder)
            .await
            .map_err(|e| ImapError::new(format!("read cursor {folder}"), e.to_string()))?;
        let current = FolderState {
            uid_validity: uid_validity.clone(),
            uid_next,
        };
        match plan_folder_sync(cursor.as_ref(), &current) {
            FolderSyncPlan::Init => {
                tracing::info!(
                    target: LOG,
                    "First run for {folder}: cursor initialized at uid {uid_next} (skipping history)"
                );
                Ok((
                    Vec::new(),
                    Some((folder.to_owned(), uid_validity, uid_next)),
                ))
            }
            FolderSyncPlan::None => Ok((Vec::new(), None)),
            FolderSyncPlan::Reset => {
                self.recover_uid_validity(client, folder, &uid_validity, uid_next)
                    .await
            }
            FolderSyncPlan::Fetch { from_uid } => {
                let (emails, next) = self
                    .fetch_new_in_folder(client, folder, &uid_validity, from_uid, uid_next)
                    .await?;
                Ok((emails, Some((folder.to_owned(), uid_validity, next))))
            }
        }
    }

    async fn fetch_new_in_folder(
        &self,
        client: &mut dyn ImapClient,
        folder: &str,
        uid_validity: &str,
        from_uid: u32,
        status_uid_next: u32,
    ) -> Result<(Vec<FetchedEmail>, u32), ImapError> {
        client
            .select(folder, true)
            .await
            .map_err(wrap(format!("lock {folder}")))?;
        let query = FetchQuery {
            internal_date: true,
            ..FetchQuery::default()
        };
        let scanned = client
            .uid_fetch(&UidSet::From(from_uid), query)
            .await
            .map_err(wrap(format!("scan {folder}")))?;
        let mut metas: Vec<(u32, Option<i64>)> = scanned
            .into_iter()
            .filter(|m| m.uid >= from_uid)
            .map(|m| (m.uid, m.internal_date_ms))
            .collect();
        metas.sort_by_key(|(uid, _)| *uid);
        metas.dedup_by_key(|(uid, _)| *uid);
        let complete = metas.len() <= MAX_EMAILS_PER_PASS;
        metas.truncate(MAX_EMAILS_PER_PASS + 1);

        let cutoff = self.now() - MAX_EMAIL_AGE_MS;
        let fresh: Vec<(u32, Option<i64>)> = metas
            .iter()
            .copied()
            .filter(|(_, date)| date.is_none_or(|d| d >= cutoff))
            .collect();
        if fresh.len() < metas.len() {
            let skipped = metas.len() - fresh.len();
            let repeat = {
                let mut state = lock(&self.inner.state);
                let repeat = state.last_skip_counts.get(folder) == Some(&skipped);
                state.last_skip_counts.insert(folder.to_owned(), skipped);
                repeat
            };
            let message = format!(
                "{folder}: skipping {skipped} message(s) older than {}d (bulk import guard)",
                MAX_EMAIL_AGE_MS / 86_400_000
            );
            if repeat {
                tracing::debug!(target: LOG, "{message}");
            } else {
                tracing::info!(target: LOG, "{message}");
            }
        } else {
            lock(&self.inner.state).last_skip_counts.remove(folder);
        }
        let selected: Vec<(u32, Option<i64>)> =
            fresh.iter().copied().take(MAX_EMAILS_PER_PASS).collect();
        if fresh.len() > selected.len() {
            tracing::warn!(
                target: LOG,
                "{folder}: fetch pass hit the {MAX_EMAILS_PER_PASS}-email cap; the rest follows on the next pass"
            );
        }
        let mut emails = Vec::new();
        for (uid, date) in &selected {
            if let Some(email) = self
                .fetch_mapped_message(client, folder, uid_validity, *uid, *date, false)
                .await?
            {
                emails.push(email);
            }
        }
        let next_uid = if !complete || fresh.len() > selected.len() {
            selected
                .last()
                .or(metas.last())
                .map_or(from_uid, |(uid, _)| uid + 1)
        } else {
            status_uid_next.max(metas.last().map_or(0, |(uid, _)| *uid) + 1)
        };
        tracing::debug!(target: LOG, "Fetched {} new email(s) from {folder}", emails.len());
        Ok((emails, next_uid))
    }

    async fn recover_uid_validity(
        &self,
        client: &mut dyn ImapClient,
        folder: &str,
        uid_validity: &str,
        uid_next: u32,
    ) -> Result<(Vec<FetchedEmail>, Option<(String, String, u32)>), ImapError> {
        let last = last_dispatched_at(&self.inner.store)
            .await
            .map_err(|e| ImapError::new("read dispatch watermark", e.to_string()))?;
        let Some(last) = last else {
            tracing::warn!(
                target: LOG,
                "{folder}: UIDVALIDITY changed with no last-dispatch timestamp; resetting cursor only"
            );
            return Ok((
                Vec::new(),
                Some((folder.to_owned(), uid_validity.to_owned(), uid_next)),
            ));
        };
        let since = last - 60 * 60_000;
        client
            .select(folder, true)
            .await
            .map_err(wrap(format!("lock {folder}")))?;
        let found = client
            .uid_search(&SearchCriteria {
                since_ms: Some(since),
                ..SearchCriteria::default()
            })
            .await
            .map_err(wrap(format!("search {folder}")))?
            .unwrap_or_default();
        let from = found.iter().copied().min().unwrap_or(uid_next);
        let (emails, next) = self
            .fetch_new_in_folder(client, folder, uid_validity, from, uid_next)
            .await?;
        tracing::warn!(
            target: LOG,
            "{folder}: UIDVALIDITY changed; recovered {} email(s) received since {}",
            emails.len(),
            omni_core::js::to_iso_string(since)
        );
        Ok((
            emails,
            Some((folder.to_owned(), uid_validity.to_owned(), next)),
        ))
    }

    async fn fetch_mapped_message(
        &self,
        client: &mut dyn ImapClient,
        folder: &str,
        uid_validity: &str,
        uid: u32,
        fallback_date: Option<i64>,
        fresh: bool,
    ) -> Result<Option<FetchedEmail>, ImapError> {
        let key = message_cache_key(folder, uid_validity, uid);
        if !fresh && let Some(cached) = lock(&self.inner.caches).parsed.get(&key, self.now()) {
            return Ok(Some(cached));
        }
        let full = fetch_one(client, uid, FetchQuery::source_and_date())
            .await
            .map_err(wrap(format!("fetch uid {uid}")))?;
        let Some((source, internal_date)) =
            full.and_then(|m| m.source.map(|s| (s, m.internal_date_ms)))
        else {
            return Ok(None);
        };
        let parsed = parse_message(&source, self.now())
            .map_err(|e| ImapError::new(format!("parse uid {uid}"), e.to_string()))?;
        let coords = MessageCoords {
            folder: folder.to_owned(),
            uid_validity: uid_validity.to_owned(),
            uid,
        };
        let email = map_parsed_message(
            &parsed,
            &coords,
            internal_date.or(fallback_date),
            self.inner.enricher.as_ref(),
        );
        lock(&self.inner.caches).parsed.set(
            &key,
            email.clone(),
            estimate_bytes(&email),
            self.now(),
        );
        tracing::debug!(
            target: LOG,
            "Email read parsed uid={uid} folder={folder} attachments={}",
            email.attachments.len()
        );
        Ok(Some(email))
    }

    // ---- reads -----------------------------------------------------------

    /// Re-fetches one email by stable id; `None` when it is gone.
    pub async fn fetch_email_by_id(
        &self,
        id: &str,
        fresh: bool,
    ) -> Result<Option<FetchedEmail>, ImapError> {
        let started = self.now();
        let cached = if fresh {
            None
        } else {
            lock(&self.inner.caches).direct.get(id, started)
        };
        let from_cache = cached.is_some();
        let email = match cached {
            Some(email) => Some(email),
            None => self.fetch_by_id_serialized(id, fresh).await?,
        };
        let ended = self.now();
        tracing::info!(
            target: LOG,
            "Email read completed source={} results={} elapsedMs={}",
            if from_cache { "cache" } else { "imap" },
            usize::from(email.is_some()),
            ended - started
        );
        Ok(email)
    }

    async fn fetch_by_id_serialized(
        &self,
        id: &str,
        fresh: bool,
    ) -> Result<Option<FetchedEmail>, ImapError> {
        let mut op = self.begin().await;
        let result = async {
            if !fresh && let Some(cached) = lock(&self.inner.caches).direct.get(id, self.now()) {
                return Ok(Some(cached));
            }
            let client = op.client()?;
            let coords = decode_message_id(id);
            let mut folders: Vec<String> = match &coords {
                Some(c) => vec![c.folder.clone()],
                None => FOLDERS.iter().map(|f| (*f).to_owned()).collect(),
            };
            let mut index = 0;
            while index < folders.len() {
                let folder = folders[index].clone();
                index += 1;
                if let Some(email) = self
                    .find_in_folder(client, &folder, id, coords.as_ref(), fresh)
                    .await?
                {
                    lock(&self.inner.caches).direct.set(
                        id,
                        email.clone(),
                        estimate_bytes(&email),
                        self.now(),
                    );
                    return Ok(Some(email));
                }
                // Discover Sent only after the usual folders miss.
                if coords.is_none() && folder == FOLDERS[FOLDERS.len() - 1] {
                    let listed = client
                        .list()
                        .await
                        .map_err(wrap("discover Sent for direct read"))?;
                    let sent: Vec<_> = listed
                        .iter()
                        .filter(|f| f.special_use_is("\\sent"))
                        .collect();
                    if sent.len() == 1 && !folders.contains(&sent[0].path) {
                        folders.push(sent[0].path.clone());
                    }
                }
            }
            Ok(None)
        }
        .await;
        op.finish().await;
        result
    }

    async fn find_in_folder(
        &self,
        client: &mut dyn ImapClient,
        folder: &str,
        id: &str,
        coords: Option<&MessageCoords>,
        fresh: bool,
    ) -> Result<Option<FetchedEmail>, ImapError> {
        client
            .select(folder, true)
            .await
            .map_err(wrap(format!("lock {folder}")))?;
        let validity = selected_validity(client).unwrap_or_else(|| "undefined".to_owned());
        let mut uid: Option<u32> = None;
        if let Some(coords) = coords {
            if coords.uid_validity != validity {
                return Ok(None);
            }
            uid = Some(coords.uid);
        } else {
            let located = if fresh {
                None
            } else {
                lock(&self.inner.caches).location.get(id, self.now())
            };
            match located {
                Some(located) if located.folder == folder && located.uid_validity == validity => {
                    let exists = fetch_one(client, located.uid, FetchQuery::UID_ONLY)
                        .await
                        .map_err(wrap(format!("check uid {}", located.uid)))?;
                    if exists.is_some() {
                        uid = Some(located.uid);
                    } else {
                        lock(&self.inner.caches).location.delete(id);
                    }
                }
                Some(_) => lock(&self.inner.caches).location.delete(id),
                None => {}
            }
            if uid.is_none() {
                let found = client
                    .uid_search(&SearchCriteria::message_id(id))
                    .await
                    .map_err(wrap(format!("find message {id}")))?
                    .unwrap_or_default();
                if !found.is_empty() {
                    if found.len() > 50 {
                        return Err(err(
                            "find exact Message-ID",
                            "Too many substring matches for a bounded direct read",
                        ));
                    }
                    for candidate in found.iter().rev() {
                        let email = self
                            .fetch_mapped_message(
                                client, folder, &validity, *candidate, None, fresh,
                            )
                            .await?;
                        let Some(email) = email.filter(|e| e.message_id.as_deref() == Some(id))
                        else {
                            continue;
                        };
                        if !fresh {
                            let location = MessageCoords {
                                folder: folder.to_owned(),
                                uid_validity: validity.clone(),
                                uid: *candidate,
                            };
                            let bytes = id.len() + folder.len() + validity.len() + 16;
                            lock(&self.inner.caches)
                                .location
                                .set(id, location, bytes, self.now());
                        }
                        return Ok(Some(email));
                    }
                    return Ok(None);
                }
            }
        }
        let Some(uid) = uid else { return Ok(None) };
        let key = message_cache_key(folder, &validity, uid);
        let cached = if fresh {
            None
        } else {
            lock(&self.inner.caches).parsed.get(&key, self.now())
        };
        if let Some(cached) = cached {
            let exists = fetch_one(client, uid, FetchQuery::UID_ONLY)
                .await
                .map_err(wrap(format!("check uid {uid}")))?;
            if exists.is_some() && (coords.is_some() || cached.message_id.as_deref() == Some(id)) {
                return Ok(Some(cached));
            }
            lock(&self.inner.caches).parsed.delete(&key);
            return Ok(None);
        }
        let email = self
            .fetch_mapped_message(client, folder, &validity, uid, None, fresh)
            .await?;
        // HEADER SEARCH is substring based; never return another message.
        if coords.is_none() && email.as_ref().and_then(|e| e.message_id.as_deref()) != Some(id) {
            lock(&self.inner.caches).location.delete(id);
            return Ok(None);
        }
        Ok(email)
    }

    /// Searches the monitored mailboxes (newest first, merged by receive time).
    pub async fn search_emails(
        &self,
        options: &EmailSearch,
    ) -> Result<Vec<FetchedEmail>, ImapError> {
        let started = self.now();
        let key = search_cache_key(options);
        if options.fresh {
            lock(&self.inner.caches).search.delete(&key);
        }
        let cached = if options.fresh {
            None
        } else {
            lock(&self.inner.caches).search.get(&key, started)
        };
        let from_cache = cached.is_some();
        let emails = match cached {
            Some(emails) => emails,
            None => self.search_serialized(options, &key).await?,
        };
        let ended = self.now();
        tracing::info!(
            target: LOG,
            "Email search completed source={} results={} elapsedMs={}",
            if from_cache { "cache" } else { "imap" },
            emails.len(),
            ended - started
        );
        Ok(emails)
    }

    async fn search_serialized(
        &self,
        options: &EmailSearch,
        key: &str,
    ) -> Result<Vec<FetchedEmail>, ImapError> {
        let mut op = self.begin().await;
        let result = async {
            if !options.fresh
                && let Some(cached) = lock(&self.inner.caches).search.get(key, self.now())
            {
                return Ok(cached);
            }
            let client = op.client()?;
            let folders: Vec<String> = match options.folder {
                Some(EmailFolderScope::Inbox) => vec!["INBOX".to_owned()],
                Some(EmailFolderScope::Archive) => vec!["Archive".to_owned()],
                Some(EmailFolderScope::Sent) => {
                    let listed = client.list().await.map_err(wrap("discover Sent mailbox"))?;
                    let sent: Vec<String> = listed
                        .iter()
                        .filter(|f| f.special_use_is("\\sent"))
                        .map(|f| f.path.clone())
                        .collect();
                    if sent.len() != 1 {
                        return Err(err(
                            "discover Sent mailbox",
                            "Expected exactly one server-designated Sent mailbox",
                        ));
                    }
                    sent
                }
                Some(EmailFolderScope::All) | None => {
                    FOLDERS.iter().map(|f| (*f).to_owned()).collect()
                }
            };
            let per_folder = usize::try_from(options.limit.clamp(1, 50)).unwrap_or(50);
            let criteria = SearchCriteria {
                text: non_empty(&options.query),
                from: non_empty(&options.from),
                to: non_empty(&options.to),
                subject: non_empty(&options.subject),
                seen: options.unread.map(|unread| !unread),
                since_ms: options.since_ms,
                before_ms: options.before_ms,
                header: options
                    .references
                    .clone()
                    .filter(|id| !id.trim().is_empty())
                    .map(|id| ("References".to_owned(), id)),
            };
            let mut merged: Vec<FetchedEmail> = Vec::new();
            for folder in &folders {
                client
                    .select(folder, true)
                    .await
                    .map_err(wrap(format!("lock {folder}")))?;
                let found = client
                    .uid_search(&criteria)
                    .await
                    .map_err(wrap(format!("search {folder}")))?;
                let Some(found) = found else { continue };
                // SEARCH is ascending: read only the newest bounded slice.
                let uids: Vec<u32> = found.iter().rev().take(per_folder).copied().collect();
                let validity = selected_validity(client).unwrap_or_else(|| "undefined".to_owned());
                let now = self.now();
                let mut by_uid: HashMap<u32, FetchedEmail> = HashMap::new();
                if !options.fresh {
                    let mut caches = lock(&self.inner.caches);
                    for uid in &uids {
                        if let Some(cached) = caches
                            .parsed
                            .get(&message_cache_key(folder, &validity, *uid), now)
                        {
                            by_uid.insert(*uid, cached);
                        }
                    }
                }
                let uncached: Vec<u32> = uids
                    .iter()
                    .copied()
                    .filter(|uid| !by_uid.contains_key(uid))
                    .collect();
                if !uncached.is_empty() {
                    let fetched = client
                        .uid_fetch(
                            &UidSet::List(uncached.clone()),
                            FetchQuery::source_and_date(),
                        )
                        .await
                        .map_err(wrap(format!("fetch {} messages", uncached.len())))?;
                    for message in fetched {
                        let Some(source) = message.source else {
                            continue;
                        };
                        let parsed = parse_message(&source, self.now())
                            .map_err(|e| ImapError::new("parse message", e.to_string()))?;
                        let coords = MessageCoords {
                            folder: folder.clone(),
                            uid_validity: validity.clone(),
                            uid: message.uid,
                        };
                        let email = map_parsed_message(
                            &parsed,
                            &coords,
                            message.internal_date_ms,
                            self.inner.enricher.as_ref(),
                        );
                        lock(&self.inner.caches).parsed.set(
                            &message_cache_key(folder, &validity, message.uid),
                            email.clone(),
                            estimate_bytes(&email),
                            self.now(),
                        );
                        tracing::debug!(
                            target: LOG,
                            "Email read parsed uid={} folder={folder} bytes={}",
                            message.uid,
                            source.len()
                        );
                        by_uid.insert(message.uid, email);
                    }
                }
                merged.extend(uids.iter().filter_map(|uid| by_uid.remove(uid)));
            }
            merged.sort_by(
                |a, b| match (iso_ms(&a.received_at), iso_ms(&b.received_at)) {
                    (Some(a), Some(b)) => b.cmp(&a),
                    _ => std::cmp::Ordering::Equal,
                },
            );
            merged.truncate(usize::try_from(options.limit).unwrap_or(usize::MAX));
            lock(&self.inner.caches).search.set(
                key,
                merged.clone(),
                estimate_bytes(&merged),
                self.now(),
            );
            Ok(merged)
        }
        .await;
        op.finish().await;
        result
    }

    // ---- attachments -----------------------------------------------------

    /// Bounded private read by stable Message-ID and MIME part identity; never marks read.
    pub async fn fetch_attachment_by_id(
        &self,
        message_id: &str,
        attachment_id: &str,
        max_bytes: Option<usize>,
    ) -> Result<Option<DownloadedAttachment>, ImapError> {
        let max_bytes = max_bytes.unwrap_or(MAX_ATTACHMENT_BYTES);
        if !valid_attachment_message_id(message_id)
            || !valid_stable_attachment_id(attachment_id)
            || !(1..=MAX_ATTACHMENT_BYTES).contains(&max_bytes)
        {
            return Err(err(
                "validate attachment request",
                "Invalid attachment identity or byte limit",
            ));
        }
        let mut op = self.begin().await;
        let result = async {
            let client = op.client()?;
            let mut folders: Vec<String> = FOLDERS.iter().map(|f| (*f).to_owned()).collect();
            let mut index = 0;
            while index < folders.len() {
                let folder = folders[index].clone();
                index += 1;
                client
                    .select(&folder, true)
                    .await
                    .map_err(wrap("lock attachment mailbox"))?;
                if let Some(found) = self
                    .attachment_in_selected(client, message_id, attachment_id, max_bytes)
                    .await?
                {
                    return Ok(Some(found));
                }
                if folder == FOLDERS[FOLDERS.len() - 1] {
                    let listed = client
                        .list()
                        .await
                        .map_err(wrap("discover Sent attachment mailbox"))?;
                    let sent: Vec<_> = listed
                        .iter()
                        .filter(|f| f.special_use_is("\\sent"))
                        .collect();
                    if sent.len() == 1 && !folders.contains(&sent[0].path) {
                        folders.push(sent[0].path.clone());
                    }
                }
            }
            Ok(None)
        }
        .await;
        op.finish().await;
        result
    }

    async fn attachment_in_selected(
        &self,
        client: &mut dyn ImapClient,
        message_id: &str,
        attachment_id: &str,
        max_bytes: usize,
    ) -> Result<Option<DownloadedAttachment>, ImapError> {
        let found = client
            .uid_search(&SearchCriteria::message_id(message_id))
            .await
            .map_err(wrap("find attachment message"))?;
        let Some(found) = found else { return Ok(None) };
        if found.len() > 50 {
            return Err(err(
                "find attachment message",
                "Too many matches for bounded attachment retrieval",
            ));
        }
        for uid in found.iter().rev() {
            let meta_query = FetchQuery {
                size: true,
                envelope: true,
                ..FetchQuery::default()
            };
            let meta = fetch_one(client, *uid, meta_query)
                .await
                .map_err(wrap("preflight attachment message"))?;
            let Some(meta) = meta.filter(|m| m.envelope_message_id.as_deref() == Some(message_id))
            else {
                continue;
            };
            let size = meta.size.unwrap_or(0);
            if size < 1 || size > MAX_ATTACHMENT_MESSAGE_BYTES as u64 {
                return Err(err(
                    "bound attachment message",
                    "Message exceeds attachment retrieval byte limit",
                ));
            }
            let source_query = FetchQuery {
                source: Some(SourceRange::Prefix {
                    max_length: MAX_ATTACHMENT_MESSAGE_BYTES + 1,
                }),
                ..FetchQuery::default()
            };
            let full = fetch_one(client, *uid, source_query)
                .await
                .map_err(wrap("fetch bounded attachment message"))?;
            let Some(source) = full.and_then(|m| m.source) else {
                continue;
            };
            if source.len() > MAX_ATTACHMENT_MESSAGE_BYTES {
                return Err(err(
                    "bound attachment source",
                    "Message exceeds attachment retrieval byte limit",
                ));
            }
            let parsed = parse_message(&source, self.now())
                .map_err(|e| ImapError::new("parse bounded attachment message", e.to_string()))?;
            if parsed.message_id.as_deref() != Some(message_id) {
                continue;
            }
            let matches: Vec<_> = parsed
                .attachments
                .iter()
                .filter(|part| {
                    attachment_part_id(part).is_some_and(|part_id| {
                        encode_stable_attachment_id(message_id, &part_id) == attachment_id
                    })
                })
                .collect();
            if matches.is_empty() {
                continue;
            }
            if matches.len() != 1 {
                return Err(err(
                    "validate MIME attachment identity",
                    "Ambiguous MIME attachment identity",
                ));
            }
            let part = matches[0];
            if part.content.len() > max_bytes {
                return Err(err(
                    "bound attachment bytes",
                    "Attachment exceeds requested byte limit",
                ));
            }
            return Ok(Some(DownloadedAttachment {
                name: safe_attachment_filename(part.filename.as_deref()),
                mime_type: declared_attachment_mime_type(part),
                data: part.content.clone(),
            }));
        }
        Ok(None)
    }

    /// Downloads one attachment by its folder/UID handle; `None` when unavailable.
    pub async fn download_attachment(
        &self,
        attachment: &EmailAttachment,
    ) -> Result<Option<DownloadedAttachment>, ImapError> {
        let Some(target) = decode_attachment_blob_id(&attachment.blob_id) else {
            tracing::warn!(target: LOG, "Unrecognized attachment handle: {}", attachment.blob_id);
            return Ok(None);
        };
        let mut op = self.begin().await;
        let result = async {
            let client = op.client()?;
            let folder = &target.coords.folder;
            client
                .select(folder, true)
                .await
                .map_err(wrap(format!("lock {folder}")))?;
            if selected_validity(client).as_deref() != Some(target.coords.uid_validity.as_str()) {
                tracing::warn!(
                    target: LOG,
                    "Attachment \"{}\" unavailable: {folder} UIDVALIDITY changed",
                    attachment.name
                );
                return Ok(None);
            }
            let query = FetchQuery {
                source: Some(SourceRange::Full),
                ..FetchQuery::default()
            };
            let full = fetch_one(client, target.coords.uid, query)
                .await
                .map_err(wrap("fetch attachment message"))?;
            let Some(source) = full.and_then(|m| m.source) else {
                tracing::warn!(
                    target: LOG,
                    "Attachment \"{}\" unavailable: message uid={} is gone",
                    attachment.name,
                    target.coords.uid
                );
                return Ok(None);
            };
            let parsed = parse_message(&source, self.now())
                .map_err(|e| ImapError::new("parse attachment message", e.to_string()))?;
            let Some(part) = parsed.attachments.get(target.index) else {
                tracing::warn!(
                    target: LOG,
                    "Attachment \"{}\" unavailable: part {} missing",
                    attachment.name,
                    target.index
                );
                return Ok(None);
            };
            Ok(Some(DownloadedAttachment {
                name: part
                    .filename
                    .clone()
                    .unwrap_or_else(|| attachment.name.clone()),
                mime_type: part.content_type.clone(),
                data: part.content.clone(),
            }))
        }
        .await;
        op.finish().await;
        result
    }

    // ---- compose -----------------------------------------------------------

    /// Saves or reconciles a private Sent copy; never submits SMTP.
    pub async fn save_sent_copy(
        &self,
        input: &SentCopyInput,
        allow_append: bool,
        before_append: Option<BeforeAppend>,
    ) -> Result<SentCopyResult, ImapError> {
        let mut op = self.begin().await;
        let now = self.now();
        let result = async {
            let client = op.client()?;
            append_sent_copy(client, input, allow_append, before_append, now).await
        }
        .await;
        if result.is_ok() {
            self.clear_read_caches();
        }
        op.finish().await;
        result
    }

    /// Saves a draft in the server-designated Drafts mailbox.
    pub async fn create_draft(
        &self,
        input: &EmailDraftInput,
        allow_append: bool,
    ) -> Result<EmailDraftResult, ImapError> {
        let mut op = self.begin().await;
        let now = self.now();
        let result = async {
            let client = op.client()?;
            create_draft(client, input, allow_append, now).await
        }
        .await;
        op.finish().await;
        result
    }

    // ---- archive -----------------------------------------------------------

    async fn archive_read<T>(
        &self,
        mutates: bool,
        run: impl for<'c> FnOnce(&'c mut dyn ImapClient) -> BoxFuture<'c, Result<T, ImapError>>,
    ) -> Result<T, ImapError> {
        let mut op = self.begin().await;
        let result = match op.client() {
            Ok(client) => run(client).await,
            Err(e) => Err(e),
        };
        if mutates {
            self.clear_read_caches();
        }
        op.finish().await;
        result
    }
}

impl ArchiveMailbox for ImapTransport {
    fn inspect<'a>(
        &'a self,
        source: &'a ArchiveSourceRequest,
    ) -> BoxFuture<'a, Result<ArchiveSnapshot, ImapError>> {
        let source = source.clone();
        Box::pin(self.archive_read(false, move |c| {
            Box::pin(async move { archive_ops::inspect_archive_source(c, &source).await })
        }))
    }

    fn move_message<'a>(
        &'a self,
        source: &'a ArchiveSourceRequest,
        hash: &'a str,
    ) -> BoxFuture<'a, Result<ArchiveMoveResult, ImapError>> {
        let (source, hash) = (source.clone(), hash.to_owned());
        Box::pin(self.archive_read(true, move |c| {
            Box::pin(async move { archive_ops::move_archive_message(c, &source, &hash).await })
        }))
    }

    fn reconcile<'a>(
        &'a self,
        source: &'a ArchiveSourceRequest,
        hash: &'a str,
    ) -> BoxFuture<'a, Result<ArchiveReconcileResult, ImapError>> {
        let (source, hash) = (source.clone(), hash.to_owned());
        Box::pin(self.archive_read(false, move |c| {
            Box::pin(async move { archive_ops::reconcile_archive_message(c, &source, &hash).await })
        }))
    }

    fn restore<'a>(
        &'a self,
        identity: &'a ArchiveIdentity,
        destination: &'a ArchiveLocation,
        hash: &'a str,
    ) -> BoxFuture<'a, Result<ArchiveMoveResult, ImapError>> {
        let (identity, destination, hash) =
            (identity.clone(), destination.clone(), hash.to_owned());
        Box::pin(self.archive_read(true, move |c| {
            Box::pin(async move {
                archive_ops::restore_archive_message(c, &identity, &destination, &hash).await
            })
        }))
    }

    fn inspect_destination<'a>(
        &'a self,
        identity: &'a ArchiveIdentity,
        destination: &'a ArchiveLocation,
    ) -> BoxFuture<'a, Result<ArchiveSnapshot, ImapError>> {
        let (identity, destination) = (identity.clone(), destination.clone());
        Box::pin(self.archive_read(false, move |c| {
            Box::pin(async move {
                archive_ops::inspect_archive_destination(c, &identity, &destination).await
            })
        }))
    }

    fn reconcile_restore<'a>(
        &'a self,
        identity: &'a ArchiveIdentity,
        destination: &'a ArchiveLocation,
        hash: &'a str,
    ) -> BoxFuture<'a, Result<ArchiveReconcileResult, ImapError>> {
        let (identity, destination, hash) =
            (identity.clone(), destination.clone(), hash.to_owned());
        Box::pin(self.archive_read(false, move |c| {
            Box::pin(async move {
                archive_ops::reconcile_restore_message(c, &identity, &destination, &hash).await
            })
        }))
    }

    fn verify<'a>(
        &'a self,
        location: &'a ArchiveLocation,
        message_id: &'a str,
        hash: &'a str,
        flags: &'a [String],
    ) -> BoxFuture<'a, Result<bool, ImapError>> {
        let (location, message_id, hash, flags) = (
            location.clone(),
            message_id.to_owned(),
            hash.to_owned(),
            flags.to_vec(),
        );
        Box::pin(self.archive_read(false, move |c| {
            Box::pin(async move {
                archive_ops::verify_archive_location(c, &location, &message_id, &hash, &flags).await
            })
        }))
    }

    fn copy<'a>(
        &'a self,
        source: &'a ArchiveSourceRequest,
        target: &'a str,
        snapshot: &'a ArchiveSnapshot,
    ) -> BoxFuture<'a, Result<ArchiveLocation, ImapError>> {
        let (source, target, snapshot) = (source.clone(), target.to_owned(), snapshot.clone());
        Box::pin(self.archive_read(true, move |c| {
            Box::pin(async move {
                archive_ops::copy_exact_archive_message(c, &source, &target, &snapshot).await
            })
        }))
    }

    fn reconcile_copy<'a>(
        &'a self,
        source: &'a ArchiveSourceRequest,
        target: &'a str,
        snapshot: &'a ArchiveSnapshot,
    ) -> BoxFuture<'a, Result<ArchiveReconcileResult, ImapError>> {
        let (source, target, snapshot) = (source.clone(), target.to_owned(), snapshot.clone());
        Box::pin(self.archive_read(false, move |c| {
            Box::pin(async move {
                archive_ops::reconcile_exact_copy(c, &source, &target, &snapshot).await
            })
        }))
    }

    fn mark_deleted<'a>(
        &'a self,
        source: &'a ArchiveIdentity,
        destination: &'a ArchiveLocation,
        snapshot: &'a ArchiveSnapshot,
    ) -> BoxFuture<'a, Result<bool, ImapError>> {
        let (source, destination, snapshot) =
            (source.clone(), destination.clone(), snapshot.clone());
        Box::pin(self.archive_read(true, move |c| {
            Box::pin(async move {
                archive_ops::mark_exact_archive_source_deleted(c, &source, &destination, &snapshot)
                    .await
            })
        }))
    }

    fn inspect_deleted<'a>(
        &'a self,
        source: &'a ArchiveIdentity,
        destination: &'a ArchiveLocation,
        snapshot: &'a ArchiveSnapshot,
    ) -> BoxFuture<'a, Result<DeletedSourceState, ImapError>> {
        let (source, destination, snapshot) =
            (source.clone(), destination.clone(), snapshot.clone());
        Box::pin(self.archive_read(false, move |c| {
            Box::pin(async move {
                archive_ops::inspect_exact_deleted_source(c, &source, &destination, &snapshot).await
            })
        }))
    }

    fn expunge<'a>(
        &'a self,
        source: &'a ArchiveIdentity,
        destination: &'a ArchiveLocation,
        snapshot: &'a ArchiveSnapshot,
    ) -> BoxFuture<'a, Result<bool, ImapError>> {
        let (source, destination, snapshot) =
            (source.clone(), destination.clone(), snapshot.clone());
        Box::pin(self.archive_read(true, move |c| {
            Box::pin(async move {
                archive_ops::expunge_exact_archive_source(c, &source, &destination, &snapshot).await
            })
        }))
    }
}

fn poll_error(error: ImapError) -> PollError {
    PollError::Transport {
        message: error.to_string(),
        transient: true,
    }
}

impl MailSource for ImapTransport {
    fn poll(&self) -> BoxFuture<'_, Result<EmailPoll, PollError>> {
        Box::pin(async move { self.poll_new_emails().await.map_err(poll_error) })
    }

    fn mail_events(&self) -> broadcast::Receiver<()> {
        self.inner.events.subscribe()
    }

    fn start(&self) -> BoxFuture<'_, Result<(), PollError>> {
        Box::pin(async move { ImapTransport::start(self).await.map_err(poll_error) })
    }

    fn stop(&self) -> BoxFuture<'_, ()> {
        Box::pin(ImapTransport::stop(self))
    }
}

fn port_error(error: ImapError) -> PortError {
    PortError::Failed {
        message: error.to_string(),
        transient: true,
    }
}

impl EmailReader for ImapTransport {
    fn fetch_by_id<'a>(
        &'a self,
        id: &'a str,
        fresh: bool,
    ) -> BoxFuture<'a, Result<Option<FetchedEmail>, PortError>> {
        Box::pin(async move { self.fetch_email_by_id(id, fresh).await.map_err(port_error) })
    }

    fn search<'a>(
        &'a self,
        q: &'a EmailSearch,
    ) -> BoxFuture<'a, Result<Vec<FetchedEmail>, PortError>> {
        Box::pin(async move { self.search_emails(q).await.map_err(port_error) })
    }

    fn health(&self) -> EmailReaderHealth {
        EmailReaderHealth {
            transport: self.name().to_owned(),
            search_available: true,
            drafts_available: true,
        }
    }

    fn download_attachment<'a>(
        &'a self,
        attachment: &'a EmailAttachment,
    ) -> BoxFuture<'a, Result<Option<DownloadedAttachment>, PortError>> {
        Box::pin(async move {
            ImapTransport::download_attachment(self, attachment)
                .await
                .map_err(port_error)
        })
    }
}

/// Archive-action detection over the archive receipts.
pub struct StoreArchiveEcho {
    store: Store,
}

impl StoreArchiveEcho {
    pub fn new(store: Store) -> Self {
        Self { store }
    }
}

impl ArchiveEcho for StoreArchiveEcho {
    fn is_archive_action_message<'a>(
        &'a self,
        message_id: &'a str,
        origin: Option<&'a omni_core::email::EmailOrigin>,
    ) -> BoxFuture<'a, Result<bool, PortError>> {
        Box::pin(async move {
            let origin = origin.map(|o| ArchiveLocation {
                folder: o.folder.clone(),
                uid_validity: o.uid_validity.clone(),
                uid: o.uid,
            });
            is_archive_action_message(&self.store, message_id, origin.as_ref())
                .await
                .map_err(|e| PortError::Failed {
                    message: e.to_string(),
                    transient: false,
                })
        })
    }
}
