//! Scheduled tasks: `McpEventDelivery` (30 s outbox sweep),
//! `ClaudeSessionEvents` (15 s host poll while subscribed),
//! `TaskRunEvents` (30 s run history scan while subscribed),
//! `McpGatewayRefresh` (hourly integration catalog refresh) and
//! `McpGatewayEventFeeds` (15 s integration event polling while subscribed).

use std::time::Duration;

use futures::future::BoxFuture;
use omni_tasks::{CronSchedule, RunContext, Task, TaskError, TaskOptions};

use crate::events::claude_sessions::ClaudeSessionWatcher;
use crate::events::service::McpEventService;
use crate::events::task_runs::TaskRunWatcher;
use crate::gateway::Gateway;
use crate::gateway::oauth::server::AuthorizationServer;

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

/// Retries due outbox rows without changing event IDs.
pub struct McpEventDeliveryTask {
    events: McpEventService,
    schedule: CronSchedule,
}

impl McpEventDeliveryTask {
    pub fn new(events: McpEventService, schedule: CronSchedule) -> Self {
        Self { events, schedule }
    }
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
            self.events
                .drain()
                .await
                .map(|_| ())
                .map_err(TaskError::from_error)
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
