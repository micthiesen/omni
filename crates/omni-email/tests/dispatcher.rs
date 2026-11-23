//! The email dispatcher, plus source-event forwarding. The dispatch watermark
//! is read back from the store.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures::future::BoxFuture;
use omni_core::email::{EmailHandler, FetchedEmail, HandlerError};
use omni_core::mail_source::{EmailPoll, MailSource, PollError};
use omni_email::dispatch_state;
use omni_email::dispatcher::EmailDispatcher;
use tokio::sync::{Notify, broadcast};
use tokio_util::task::TaskTracker;

use common::{NOW, email, store_at};

type PollFn = Box<dyn Fn(usize) -> BoxFuture<'static, Result<EmailPoll, PollError>> + Send + Sync>;

struct FakeSource {
    polls: AtomicUsize,
    poll: PollFn,
    events: broadcast::Sender<()>,
    start_fails: bool,
    /// When set, `start` signals `start_entered` and waits for this gate.
    start_gate: Option<Arc<Notify>>,
    start_entered: Arc<Notify>,
    stopped: AtomicUsize,
}

impl FakeSource {
    fn new(poll: PollFn) -> Arc<Self> {
        Arc::new(Self {
            polls: AtomicUsize::new(0),
            poll,
            events: broadcast::channel(16).0,
            start_fails: false,
            start_gate: None,
            start_entered: Arc::new(Notify::new()),
            stopped: AtomicUsize::new(0),
        })
    }
}

impl MailSource for FakeSource {
    fn poll(&self) -> BoxFuture<'_, Result<EmailPoll, PollError>> {
        let n = self.polls.fetch_add(1, Ordering::SeqCst) + 1;
        (self.poll)(n)
    }

    fn mail_events(&self) -> broadcast::Receiver<()> {
        self.events.subscribe()
    }

    fn start(&self) -> BoxFuture<'_, Result<(), PollError>> {
        let fails = self.start_fails;
        let gate = self.start_gate.clone();
        let entered = self.start_entered.clone();
        Box::pin(async move {
            if let Some(gate) = gate {
                entered.notify_one();
                gate.notified().await;
            }
            if fails {
                Err(PollError::Transport {
                    message: "connection failed".to_owned(),
                    transient: true,
                })
            } else {
                Ok(())
            }
        })
    }

    fn stop(&self) -> BoxFuture<'_, ()> {
        self.stopped.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {})
    }
}

fn poll_of(
    emails: Vec<FetchedEmail>,
    commit: impl Fn() -> Result<(), PollError> + Send + Sync + 'static,
) -> PollFn {
    let commit = Arc::new(commit);
    Box::new(move |_| {
        let emails = emails.clone();
        let commit = commit.clone();
        Box::pin(async move {
            Ok(EmailPoll {
                emails,
                commit: Box::new(move || Box::pin(async move { commit() })),
            })
        })
    })
}

struct Handler {
    name: &'static str,
    calls: AtomicUsize,
    fail: bool,
}

impl Handler {
    fn new(name: &'static str, fail: bool) -> Arc<Self> {
        Arc::new(Self {
            name,
            calls: AtomicUsize::new(0),
            fail,
        })
    }
}

impl EmailHandler for Handler {
    fn name(&self) -> &'static str {
        self.name
    }

    fn handle<'a>(
        &'a self,
        _emails: &'a [FetchedEmail],
    ) -> BoxFuture<'a, Result<(), HandlerError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let fail = self.fail;
        Box::pin(async move {
            if fail {
                Err(HandlerError::permanent("not durable", None))
            } else {
                Ok(())
            }
        })
    }
}

#[tokio::test]
async fn does_not_advance_the_dispatch_watermark_when_cursor_persistence_fails() {
    let (store, _clock) = store_at(NOW).await;
    let source = FakeSource::new(poll_of(vec![email("email-1")], || {
        Err(PollError::Commit("cursor unavailable".to_owned()))
    }));
    let ok = Handler::new("ok", false);
    let dispatcher = EmailDispatcher::new(
        store.store.clone(),
        source,
        "test",
        vec![ok],
        TaskTracker::new(),
    );
    let logs = omni_testkit::capture_logs();
    dispatcher.poll_once().await;
    assert_eq!(
        dispatch_state::last_dispatched_at(&store.store)
            .await
            .unwrap(),
        None
    );
    assert!(
        logs.events()
            .iter()
            .any(|e| e.level == tracing::Level::ERROR)
    );
}

#[tokio::test]
async fn does_not_commit_the_cursor_when_any_handler_fails() {
    let (store, _clock) = store_at(NOW).await;
    let commits = Arc::new(AtomicUsize::new(0));
    let counter = commits.clone();
    let source = FakeSource::new(poll_of(vec![email("email-1")], move || {
        counter.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }));
    let successful = Handler::new("successful", false);
    let failed = Handler::new("failed", true);
    let dispatcher = EmailDispatcher::new(
        store.store.clone(),
        source,
        "test",
        vec![successful.clone(), failed.clone()],
        TaskTracker::new(),
    );
    let logs = omni_testkit::capture_logs();
    dispatcher.poll_once().await;
    assert_eq!(failed.calls.load(Ordering::SeqCst), 1);
    assert!(
        logs.events()
            .iter()
            .any(|e| e.level == tracing::Level::ERROR)
    );
    assert_eq!(successful.calls.load(Ordering::SeqCst), 1);
    assert_eq!(commits.load(Ordering::SeqCst), 0);
    assert_eq!(
        dispatch_state::last_dispatched_at(&store.store)
            .await
            .unwrap(),
        None
    );
}

#[tokio::test(start_paused = true)]
async fn commits_only_after_every_handler_succeeds() {
    let (store, _clock) = store_at(NOW).await;
    let commits = Arc::new(AtomicUsize::new(0));
    let counter = commits.clone();
    let source = FakeSource::new(poll_of(vec![email("email-1")], move || {
        counter.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }));
    let ok = Handler::new("ok", false);
    let dispatcher = EmailDispatcher::new(
        store.store.clone(),
        source,
        "test",
        vec![ok],
        TaskTracker::new(),
    );
    dispatcher.poll_once().await;
    assert_eq!(commits.load(Ordering::SeqCst), 1);
    assert_eq!(
        dispatch_state::last_dispatched_at(&store.store)
            .await
            .unwrap(),
        Some(NOW)
    );
}

#[tokio::test]
async fn drains_a_notification_that_arrives_while_the_active_pass_is_completing() {
    let (store, _clock) = store_at(NOW).await;
    let first_started = Arc::new(Notify::new());
    let release_first = Arc::new(Notify::new());
    let second_finished = Arc::new(Notify::new());
    let (started, release, finished) = (
        first_started.clone(),
        release_first.clone(),
        second_finished.clone(),
    );
    let source = FakeSource::new(Box::new(move |n| {
        let (started, release, finished) = (started.clone(), release.clone(), finished.clone());
        Box::pin(async move {
            if n == 1 {
                started.notify_one();
                release.notified().await;
            } else {
                finished.notify_one();
            }
            Ok(EmailPoll {
                emails: Vec::new(),
                commit: Box::new(|| Box::pin(async { Ok(()) })),
            })
        })
    }));
    let dispatcher = EmailDispatcher::new(
        store.store.clone(),
        source.clone(),
        "test",
        Vec::new(),
        TaskTracker::new(),
    );
    dispatcher.start().await.unwrap();
    dispatcher.notify();
    first_started.notified().await;
    dispatcher.notify();
    dispatcher.notify();
    dispatcher.notify();

    release_first.notify_one();
    dispatcher.stop().await;
    second_finished.notified().await;
    assert_eq!(source.polls.load(Ordering::SeqCst), 2);

    dispatcher.notify();
    tokio::task::yield_now().await;
    assert_eq!(source.polls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_notification_during_a_slow_start_is_not_dropped() {
    let (store, _clock) = store_at(NOW).await;
    let polled = Arc::new(Notify::new());
    let signal = polled.clone();
    let gate = Arc::new(Notify::new());
    let mut source = FakeSource::new(Box::new(move |_| {
        let signal = signal.clone();
        Box::pin(async move {
            signal.notify_one();
            Ok(EmailPoll {
                emails: Vec::new(),
                commit: Box::new(|| Box::pin(async { Ok(()) })),
            })
        })
    }));
    Arc::get_mut(&mut source).unwrap().start_gate = Some(gate.clone());
    let dispatcher = EmailDispatcher::new(
        store.store.clone(),
        source.clone(),
        "test",
        Vec::new(),
        TaskTracker::new(),
    );
    let starting = tokio::spawn({
        let dispatcher = dispatcher.clone();
        async move { dispatcher.start().await }
    });
    source.start_entered.notified().await;
    // The lifecycle lock is held by `start` here; the event must still land.
    dispatcher.notify();
    tokio::time::timeout(Duration::from_secs(5), polled.notified())
        .await
        .expect("the pass requested during start ran");
    gate.notify_one();
    starting.await.unwrap().unwrap();
    dispatcher.stop().await;
    assert_eq!(source.polls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn stops_the_transport_and_supervisor_when_startup_fails() {
    let (store, _clock) = store_at(NOW).await;
    let mut source = FakeSource::new(poll_of(Vec::new(), || Ok(())));
    Arc::get_mut(&mut source).unwrap().start_fails = true;
    let dispatcher = EmailDispatcher::new(
        store.store.clone(),
        source.clone(),
        "test",
        Vec::new(),
        TaskTracker::new(),
    );
    let error = dispatcher.start().await.unwrap_err();
    assert!(error.to_string().contains("connection failed"));
    assert_eq!(source.stopped.load(Ordering::SeqCst), 1);

    dispatcher.notify();
    tokio::task::yield_now().await;
    assert_eq!(source.polls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn source_events_trigger_a_pass() {
    let (store, _clock) = store_at(NOW).await;
    let polled = Arc::new(Notify::new());
    let signal = polled.clone();
    let source = FakeSource::new(Box::new(move |_| {
        let signal = signal.clone();
        Box::pin(async move {
            signal.notify_one();
            Ok(EmailPoll {
                emails: Vec::new(),
                commit: Box::new(|| Box::pin(async { Ok(()) })),
            })
        })
    }));
    let dispatcher = EmailDispatcher::new(
        store.store.clone(),
        source.clone(),
        "test",
        Vec::new(),
        TaskTracker::new(),
    );
    dispatcher.start().await.unwrap();
    source.events.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), polled.notified())
        .await
        .unwrap();
    dispatcher.stop().await;
    assert!(source.polls.load(Ordering::SeqCst) >= 1);
}

#[tokio::test]
async fn the_background_service_starts_dispatches_and_stops_on_shutdown() {
    let app = omni_testkit::TestApp::new().await;
    let polled = Arc::new(Notify::new());
    let signal = polled.clone();
    let source = FakeSource::new(Box::new(move |_| {
        let signal = signal.clone();
        Box::pin(async move {
            signal.notify_one();
            Ok(EmailPoll {
                emails: vec![email("e1")],
                commit: Box::new(|| Box::pin(async { Ok(()) })),
            })
        })
    }));
    let handler = Handler::new("ok", false);
    let service = omni_email::dispatcher::service(source.clone(), "IMAP", vec![handler.clone()]);
    assert!(service.retry.is_some());
    let running = tokio::spawn((service.start)(app.ctx.clone()));
    // Wait until the source is started, then signal mail.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if source.events.send(()).is_ok() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), polled.notified())
        .await
        .unwrap();
    app.ctx.shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(5), running)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(handler.calls.load(Ordering::SeqCst), 1);
    assert_eq!(source.stopped.load(Ordering::SeqCst), 1);
}
