//! Scheduled tasks: `McpEventDelivery` (30 s outbox sweep),
//! `ClaudeSessionEvents` (15 s host poll while subscribed),
//! `TaskRunEvents` (30 s run history scan while subscribed),
//! `McpGatewayRefresh` (hourly integration catalog refresh) and
//! `McpGatewayEventFeeds` (15 s integration event polling while subscribed).

use std::time::Duration;

use futures::future::BoxFuture;
use indexmap::IndexMap;
use omni_alerts::{Pushover, PushoverChannel, PushoverMessage};
use omni_core::spawn::must_complete;
use omni_store::StoreError;
use omni_tasks::{CronSchedule, RunContext, Task, TaskError, TaskOptions};
use tokio_util::task::TaskTracker;

use crate::events::claude_sessions::ClaudeSessionWatcher;
use crate::events::service::{Lapse, McpEventService};
use crate::events::task_runs::TaskRunWatcher;
use crate::gateway::Gateway;
use crate::gateway::oauth::server::AuthorizationServer;

const LOG: &str = "MCP:Events";

pub const MCP_EVENT_DELIVERY_SCHEDULE: &str = "*/30 * * * * *";
pub const CLAUDE_SESSION_EVENTS_SCHEDULE: &str = "*/15 * * * * *";
pub const TASK_RUN_EVENTS_SCHEDULE: &str = "15,45 * * * * *";
pub const GATEWAY_REFRESH_SCHEDULE: &str = "0 7 * * * *";
pub const GATEWAY_EVENT_FEEDS_SCHEDULE: &str = "5,20,35,50 * * * * *";

fn options() -> TaskOptions {
    TaskOptions {
        jitter: Duration::ZERO,
        run_on_startup: false,
    }
}

/// Retries due outbox rows without changing event IDs, then tells the owner
/// once about each subscription that expired without a refresh.
pub struct McpEventDeliveryTask {
    events: McpEventService,
    pushover: Pushover,
    tracker: TaskTracker,
    schedule: CronSchedule,
}

impl McpEventDeliveryTask {
    pub fn new(
        events: McpEventService,
        pushover: Pushover,
        tracker: TaskTracker,
        schedule: CronSchedule,
    ) -> Self {
        Self {
            events,
            pushover,
            tracker,
            schedule,
        }
    }
}

/// Reserves lapses and sends one push per client; a failed or uncertain
/// push is logged and never resent.
async fn report_lapses(events: McpEventService, pushover: Pushover) -> Result<(), StoreError> {
    for (client, message) in lapse_messages(&events.reserve_lapses().await?) {
        match pushover.send(PushoverChannel::General, message).await {
            Ok(_) => tracing::info!(
                target: LOG,
                client = %client,
                "MCP event subscriptions lapsed unrefreshed; owner notified"
            ),
            Err(error) => tracing::warn!(
                target: LOG,
                client = %client,
                error = %error,
                "MCP event subscription lapse notification failed; not resent"
            ),
        }
    }
    Ok(())
}

/// One owner notification per client (owner and callback host), naming each
/// lapsed event and its arguments, in reservation order.
pub fn lapse_messages(lapses: &[Lapse]) -> Vec<(String, PushoverMessage)> {
    let mut clients: IndexMap<String, (Vec<String>, i64)> = IndexMap::new();
    for lapse in lapses {
        let client = match &lapse.callback_host {
            Some(host) => format!("{} at {host}", lapse.owner),
            None => lapse.owner.clone(),
        };
        let arguments: Vec<String> = lapse
            .arguments
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect();
        let event = if arguments.is_empty() {
            lapse.name.clone()
        } else {
            format!("{} ({})", lapse.name, arguments.join(", "))
        };
        let entry = clients
            .entry(client)
            .or_insert((Vec::new(), lapse.expired_at));
        entry.0.push(event);
        entry.1 = entry.1.max(lapse.expired_at);
    }
    clients
        .into_iter()
        .map(|(client, (events, expired_at))| {
            let message = PushoverMessage {
                title: Some("MCP event subscription lapsed".to_owned()),
                message: format!(
                    "{client} stopped refreshing: {}. Those events are no longer delivered \
                     until it subscribes again.",
                    events.join("; ")
                ),
                timestamp: Some(expired_at / 1000),
                ..PushoverMessage::default()
            };
            (client, message)
        })
        .collect()
}

impl Task for McpEventDeliveryTask {
    fn name(&self) -> &str {
        "McpEventDelivery"
    }

    fn schedule(&self) -> &CronSchedule {
        &self.schedule
    }

    fn options(&self) -> TaskOptions {
        options()
    }

    fn run<'a>(&'a self, _cx: &'a RunContext) -> BoxFuture<'a, Result<(), TaskError>> {
        Box::pin(async move {
            // Lapses are reported even when the delivery pass fails.
            let drained = self.events.drain().await;
            let (events, pushover) = (self.events.clone(), self.pushover.clone());
            let reported = must_complete(&self.tracker, report_lapses(events, pushover)).await;
            drained.map_err(TaskError::from_error)?;
            reported.map_err(TaskError::from_error)
        })
    }
}

/// Publishes `claude.session.turn_finished` while a subscription is active.
pub struct ClaudeSessionEventsTask {
    watcher: ClaudeSessionWatcher,
    schedule: CronSchedule,
}

impl ClaudeSessionEventsTask {
    pub fn new(watcher: ClaudeSessionWatcher, schedule: CronSchedule) -> Self {
        Self { watcher, schedule }
    }
}

impl Task for ClaudeSessionEventsTask {
    fn name(&self) -> &str {
        "ClaudeSessionEvents"
    }

    fn schedule(&self) -> &CronSchedule {
        &self.schedule
    }

    fn options(&self) -> TaskOptions {
        options()
    }

    fn run<'a>(&'a self, _cx: &'a RunContext) -> BoxFuture<'a, Result<(), TaskError>> {
        Box::pin(async move { self.watcher.poll().await.map_err(TaskError::from_error) })
    }
}

/// Publishes `task.run_finished` while a subscription is active.
pub struct TaskRunEventsTask {
    watcher: TaskRunWatcher,
    schedule: CronSchedule,
}

impl TaskRunEventsTask {
    pub fn new(watcher: TaskRunWatcher, schedule: CronSchedule) -> Self {
        Self { watcher, schedule }
    }
}

impl Task for TaskRunEventsTask {
    fn name(&self) -> &str {
        "TaskRunEvents"
    }

    fn schedule(&self) -> &CronSchedule {
        &self.schedule
    }

    fn options(&self) -> TaskOptions {
        options()
    }

    fn run<'a>(&'a self, _cx: &'a RunContext) -> BoxFuture<'a, Result<(), TaskError>> {
        Box::pin(async move {
            self.watcher
                .poll()
                .await
                .map(|_| ())
                .map_err(TaskError::from_error)
        })
    }
}

/// Refreshes every enabled gateway integration's catalog and prunes expired
/// downstream OAuth requests.
pub struct GatewayRefreshTask {
    gateway: Gateway,
    authorization: Option<AuthorizationServer>,
    schedule: CronSchedule,
}

impl GatewayRefreshTask {
    pub fn new(
        gateway: Gateway,
        authorization: Option<AuthorizationServer>,
        schedule: CronSchedule,
    ) -> Self {
        Self {
            gateway,
            authorization,
            schedule,
        }
    }
}

impl Task for GatewayRefreshTask {
    fn name(&self) -> &str {
        "McpGatewayRefresh"
    }

    fn schedule(&self) -> &CronSchedule {
        &self.schedule
    }

    fn options(&self) -> TaskOptions {
        options()
    }

    fn run<'a>(&'a self, _cx: &'a RunContext) -> BoxFuture<'a, Result<(), TaskError>> {
        Box::pin(async move {
            self.gateway.refresh_all();
            match &self.authorization {
                Some(server) => server.prune().await.map_err(TaskError::from_error),
                None => Ok(()),
            }
        })
    }
}

/// Follows integration events for current gateway subscribers.
pub struct GatewayEventFeedsTask {
    gateway: Gateway,
    events: McpEventService,
    schedule: CronSchedule,
}

impl GatewayEventFeedsTask {
    pub fn new(gateway: Gateway, events: McpEventService, schedule: CronSchedule) -> Self {
        Self {
            gateway,
            events,
            schedule,
        }
    }
}

impl Task for GatewayEventFeedsTask {
    fn name(&self) -> &str {
        "McpGatewayEventFeeds"
    }

    fn schedule(&self) -> &CronSchedule {
        &self.schedule
    }

    fn options(&self) -> TaskOptions {
        options()
    }

    fn run<'a>(&'a self, _cx: &'a RunContext) -> BoxFuture<'a, Result<(), TaskError>> {
        Box::pin(async move {
            crate::gateway::events::poll_feeds(&self.gateway, &self.events)
                .await
                .map(|_| ())
                .map_err(TaskError::from_error)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::catalog::EventArguments;

    fn lapse(name: &str, folder: Option<&str>, host: &str, expired_at: i64) -> Lapse {
        Lapse {
            name: name.to_owned(),
            arguments: folder
                .map(|f| EventArguments::from([("folder".to_owned(), f.to_owned())]))
                .unwrap_or_default(),
            owner: "gateway:grant:hG37Tj8TFUyEXG".to_owned(),
            callback_host: Some(host.to_owned()),
            expired_at,
        }
    }

    #[test]
    fn lapse_messages_send_one_push_per_client_naming_each_event() {
        let openai = "connectors.api.openai.com";
        let messages = lapse_messages(&[
            lapse("email.received", Some("inbox"), openai, 1_792_525_204_129),
            lapse(
                "task.run_finished",
                None,
                "hooks.example.com",
                1_792_525_300_000,
            ),
            lapse("email.received", Some("archive"), openai, 1_792_525_204_896),
        ]);
        assert_eq!(messages.len(), 2);
        let (client, message) = &messages[0];
        assert_eq!(
            client,
            "gateway:grant:hG37Tj8TFUyEXG at connectors.api.openai.com"
        );
        assert_eq!(
            message.title.as_deref(),
            Some("MCP event subscription lapsed")
        );
        assert_eq!(
            message.message,
            "gateway:grant:hG37Tj8TFUyEXG at connectors.api.openai.com stopped refreshing: \
             email.received (folder=inbox); email.received (folder=archive). Those events are \
             no longer delivered until it subscribes again."
        );
        assert_eq!(message.timestamp, Some(1_792_525_204));
        assert!(
            messages[1]
                .1
                .message
                .contains("task.run_finished. Those events")
        );
    }
}
