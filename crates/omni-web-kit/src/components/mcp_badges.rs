//! MCP policy tag and call status.

use leptos::prelude::*;
use omni_api::mcp_activity::{McpCallStatus, RecommendedPolicy};

use super::badges::{Status, Tag};
use super::tone::{StatusKind, Tone};

pub fn policy_str(policy: RecommendedPolicy) -> &'static str {
    match policy {
        RecommendedPolicy::Allow => "allow",
        RecommendedPolicy::RequireApproval => "require_approval",
        RecommendedPolicy::Block => "block",
    }
}

pub fn call_status_str(status: McpCallStatus) -> &'static str {
    match status {
        McpCallStatus::Running => "running",
        McpCallStatus::Ok => "ok",
        McpCallStatus::Error => "error",
        McpCallStatus::Interrupted => "interrupted",
        McpCallStatus::Invalid => "invalid",
    }
}

fn policy_label(policy: &str) -> String {
    match policy {
        "allow" => "allow".to_owned(),
        "require_approval" => "approval".to_owned(),
        "block" => "block".to_owned(),
        other => other.to_owned(),
    }
}

fn policy_tone(policy: &str) -> Tone {
    match policy {
        "require_approval" => Tone::Warn,
        "block" => Tone::Fault,
        _ => Tone::Neutral,
    }
}

/// Policy as a [`Tag`]; unknown policies show their raw value.
#[component]
pub fn PolicyBadge(#[prop(into)] policy: String) -> impl IntoView {
    let title = format!("Recommended policy: {}", policy.replace('_', " "));
    view! { <Tag tone=policy_tone(&policy) title=title>{policy_label(&policy)}</Tag> }
}

pub fn call_status_kind(status: McpCallStatus) -> StatusKind {
    match status {
        McpCallStatus::Running => StatusKind::Running,
        McpCallStatus::Ok => StatusKind::Ok,
        McpCallStatus::Error => StatusKind::Fault,
        McpCallStatus::Interrupted | McpCallStatus::Invalid => StatusKind::Warn,
    }
}

#[component]
pub fn CallStatusPill(status: McpCallStatus) -> impl IntoView {
    let label = match status {
        McpCallStatus::Running => "Running",
        McpCallStatus::Ok => "OK",
        McpCallStatus::Error => "Error",
        McpCallStatus::Interrupted => "Interrupted",
        McpCallStatus::Invalid => "Invalid input",
    };
    view! { <Status kind=call_status_kind(status) label=label/> }
}
