//! A recording `EventPublisher` that imitates the MCP Events outbox's
//! receipt dedup: the first publication of a `(name, dedup_key)` returns
//! `true`, replays return `false` and are still recorded. Target notices are
//! recorded once per key with a state tests can move.

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use omni_runtime::ports::{
    EventPublication, EventPublisher, NoticeState, NoticeStatus, PortError, Ports, TargetNotice,
};

#[derive(Default)]
struct Inner {
    published: Vec<EventPublication>,
    receipts: HashSet<(String, String)>,
    arguments: Vec<(String, BTreeMap<String, String>)>,
    fail: bool,
    notices: Vec<TargetNotice>,
    states: BTreeMap<String, NoticeState>,
    /// Targets that do not exist (their notices start `unconfigured`).
    missing: HashSet<String>,
}

/// Every publication, in order; cheap to clone.
#[derive(Clone, Default)]
pub struct RecordedEvents(Arc<Mutex<Inner>>);

impl RecordedEvents {
    /// A recorder already set as `ports`' event publisher.
    pub fn install(ports: &Ports) -> Self {
        let events = Self::default();
        let _ = ports.set_event_publisher(Arc::new(events.clone()));
        events
    }

    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Every publication, replays included.
    pub fn published(&self) -> Vec<EventPublication> {
        self.inner().published.clone()
    }

    /// Publications that were not replays of an earlier dedup key.
    pub fn distinct(&self) -> Vec<EventPublication> {
        let mut seen = HashSet::new();
        self.published()
            .into_iter()
            .filter(|event| seen.insert((event.name, event.dedup_key.clone())))
            .collect()
    }

    /// Adds an active subscription's arguments for `active_arguments`.
    pub fn subscribe(&self, name: &str, arguments: &[(&str, &str)]) {
        let arguments = arguments
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        self.inner().arguments.push((name.to_owned(), arguments));
    }

    /// Makes every later `publish` and `notify_target` fail.
    pub fn fail(&self) {
        self.inner().fail = true;
    }

    /// Undoes [`Self::fail`].
    pub fn recover(&self) {
        self.inner().fail = false;
    }

    /// Every distinct target notice, in order.
    pub fn notices(&self) -> Vec<TargetNotice> {
        self.inner().notices.clone()
    }

    /// Moves a notice to `status` (what the journal's worker would do).
    pub fn settle_notice(&self, key: &str, status: NoticeStatus, attempts: u32) {
        self.inner()
            .states
            .insert(key.to_owned(), NoticeState { status, attempts });
    }

    /// Notices to `target` start `unconfigured`.
    pub fn without_target(&self, target: &str) {
        self.inner().missing.insert(target.to_owned());
    }
}

impl EventPublisher for RecordedEvents {
    fn publish<'a>(
        &'a self,
        event: &'a EventPublication,
    ) -> BoxFuture<'a, Result<bool, PortError>> {
        let mut inner = self.inner();
        let result = if inner.fail {
            Err(PortError::Failed {
                message: "event outbox unavailable".to_owned(),
                transient: true,
            })
        } else {
            inner.published.push(event.clone());
            Ok(inner
                .receipts
                .insert((event.name.to_owned(), event.dedup_key.clone())))
        };
        Box::pin(async move { result })
    }

    fn active_arguments<'a>(
        &'a self,
        name: &'a str,
    ) -> BoxFuture<'a, Result<Vec<BTreeMap<String, String>>, PortError>> {
        let arguments = self
            .inner()
            .arguments
            .iter()
            .filter(|(event, _)| event == name)
            .map(|(_, args)| args.clone())
            .collect();
        Box::pin(async move { Ok(arguments) })
    }

    fn notify_target<'a>(&'a self, notice: TargetNotice) -> BoxFuture<'a, Result<(), PortError>> {
        let mut inner = self.inner();
        let result = if inner.fail {
            Err(PortError::Failed {
                message: "event journal unavailable".to_owned(),
                transient: true,
            })
        } else {
            if !inner.states.contains_key(&notice.key) {
                let status = if inner.missing.contains(&notice.target) {
                    NoticeStatus::Unconfigured
                } else {
                    NoticeStatus::Pending
                };
                inner.states.insert(
                    notice.key.clone(),
                    NoticeState {
                        status,
                        attempts: 0,
                    },
                );
                inner.notices.push(notice);
            }
            Ok(())
        };
        Box::pin(async move { result })
    }

    fn notice_states<'a>(
        &'a self,
        keys: Vec<String>,
    ) -> BoxFuture<'a, Result<BTreeMap<String, NoticeState>, PortError>> {
        let inner = self.inner();
        let states = keys
            .into_iter()
            .filter_map(|key| inner.states.get(&key).map(|state| (key, *state)))
            .collect();
        Box::pin(async move { Ok(states) })
    }

    fn cancel_notice<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<bool, PortError>> {
        let mut inner = self.inner();
        let cancelled = match inner.states.get_mut(key) {
            Some(state)
                if matches!(
                    state.status,
                    NoticeStatus::Pending | NoticeStatus::Unconfigured
                ) =>
            {
                state.status = NoticeStatus::Skipped;
                true
            }
            _ => false,
        };
        Box::pin(async move { Ok(cancelled) })
    }
}
