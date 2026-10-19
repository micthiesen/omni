//! The MCP Events tools' contracts: metadata, policy and the schema types
//! their input and output schemas derive from (`omni_mcp_kit::schema`). These
//! types describe the wire format only; the handlers decode and encode with
//! their own types. After changing anything here run `cargo xtask mcp-golden`
//! and review the snapshot diff.

use std::collections::BTreeMap;

use omni_mcp_kit::{Annotations, ApprovalPolicy, Policy, ToolDef, ToolDefinition, ToolInfo};
use schemars::JsonSchema;

pub static EVENTS_STATUS: ToolDef<EventsStatusInput, EventsStatusOutput> = ToolDef::new(ToolInfo {
    name: "events_status",
    title: "Check MCP Event Delivery",
    description: "Report the MCP Events lifecycle Omni has observed for every event (email.received, claude.session.turn_finished, livestream.status_changed, presspods.job_finished, task.run_finished, calendar.event_changed, calendar.event_starting): recent events/list and subscription requests, subscriptions by event, arguments and callback host, and webhook delivery outcomes. Contains no secrets, tokens, callback paths, or message content.",
    annotations: Annotations {
        read_only_hint: true,
        destructive_hint: false,
        idempotent_hint: true,
        open_world_hint: false,
    },
    policy: Policy {
        side_effects: &[],
        cost: "none",
        recommended: ApprovalPolicy::Allow,
    },
});

/// Every MCP Events tool, in serving order.
pub static TOOLS: [&dyn ToolDefinition; 1] = [&EVENTS_STATUS];

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct EventsStatusInput {}

#[derive(JsonSchema)]
pub enum Method {
    #[schemars(rename = "events/list")]
    EventsList,
    #[schemars(rename = "events/subscribe")]
    EventsSubscribe,
    #[schemars(rename = "events/unsubscribe")]
    EventsUnsubscribe,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub at: String,
    pub method: Method,
    pub owner: String,
    pub name: Option<String>,
    pub arguments: Option<BTreeMap<String, String>>,
    pub callback_host: Option<String>,
    pub outcome: String,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "snake_case")]
pub enum State {
    Active,
    Expired,
    StaleKey,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct Subscription {
    pub id: String,
    pub name: String,
    pub arguments: BTreeMap<String, String>,
    pub owner: String,
    pub callback_host: Option<String>,
    pub state: State,
    pub refresh_before: String,
    pub expires_at: String,
    pub verified_at: String,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "lowercase")]
pub enum Status {
    Pending,
    Delivered,
    Failed,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "snake_case")]
pub enum Failure {
    SubscriptionInactive,
    CredentialsUnavailable,
    Rejected,
    AttemptsExhausted,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "snake_case")]
pub enum Withheld {
    AuthorizationInvalid,
    AuthorizationUnavailable,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct RecentItem {
    pub event_id: String,
    pub subscription_id: String,
    pub name: String,
    pub status: Status,
    pub attempts: f64,
    pub last_status: Option<f64>,
    pub last_error: Option<String>,
    pub failure: Option<Failure>,
    pub withheld: Option<Withheld>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct Deliveries {
    pub pending: f64,
    pub withheld: f64,
    pub delivered: f64,
    pub failed: f64,
    pub recent: Vec<RecentItem>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventsStatusOutput {
    pub enabled: bool,
    pub checked_at: Option<String>,
    pub requests: Vec<Request>,
    pub subscription_total: f64,
    pub subscriptions: Vec<Subscription>,
    pub deliveries: Deliveries,
}
