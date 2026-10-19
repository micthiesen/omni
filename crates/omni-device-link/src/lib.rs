//! Device link: the Claude Code host's outbound long-poll relay.
//!
//! The host's `omni-link` agent long-polls `POST /device-link/poll` and posts
//! each job's output to `POST /device-link/result`, both authenticated by
//! `OMNI_DEVICE_LINK_TOKEN` only (which config validation keeps strong; it is
//! never an MCP credential). Omni never connects to the host. MCP tools
//! (`omni-mcp`) reach the host only through the `omni_runtime::ports::ClaudeHost`
//! port, which [`DeviceLink::from_context`] sets to this service.

mod host;
pub mod routes;
pub mod service;

pub use service::{
    DEVICE_ONLINE_WINDOW_MS, DEVICE_PICKUP_TIMEOUT, DEVICE_POLL_HOLD, DeviceCommand, DeviceJob,
    DeviceJobOutcome, DeviceLinkError, DeviceLinkService, DeviceLinkStatus, PollReport,
    RESULT_SLACK,
};

use std::sync::Arc;

use omni_runtime::{AppContext, Subsystem};

/// The device link for this process, or `None` when `OMNI_DEVICE_LINK_TOKEN`
/// is unset (the routes are then not mounted).
/// Building it sets the `ClaudeHost` port, so build it before `omni-mcp`'s
/// `McpPackage`.
pub struct DeviceLink {
    pub service: DeviceLinkService,
    token: String,
}

impl DeviceLink {
    pub fn from_context(ctx: &AppContext) -> Option<Self> {
        let token = ctx
            .config
            .omni_device_link_token
            .as_deref()
            .filter(|token| !token.is_empty())?;
        let service = DeviceLinkService::new(ctx.clock.clone());
        if ctx
            .ports
            .set_claude_host(Arc::new(service.clone()))
            .is_err()
        {
            tracing::error!(
                target: "DeviceLink",
                "The Claude Code host port was already set; this device link stays unmounted"
            );
            return None;
        }
        Some(Self {
            service,
            token: token.to_owned(),
        })
    }

    /// The device-link routes as a subsystem. The service itself is shared
    /// with `omni-mcp` (Claude tools, activity routes, session watcher).
    pub fn subsystem(&self) -> Subsystem {
        Subsystem {
            router: routes::router(self.service.clone(), &self.token),
            ..Subsystem::named("device-link")
        }
    }
}
