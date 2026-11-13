//! `events_status`: the MCP Events lifecycle Omni
//! has observed, without secrets, tokens, callback paths or message content.

pub mod defs;

use omni_mcp_kit::{McpTool, ToolContext, ToolError, ToolMetaError, typed_tool};
use serde::Deserialize;
use serde_json::{Value, json};

use super::conform;
use crate::events::claude_notify::ClaudeNotify;
use crate::events::service::McpEventService;

#[derive(Deserialize)]
struct EmptyInput {}

/// The status body; `None` reports MCP Events as disabled.
pub async fn events_status(
    events: Option<&McpEventService>,
    notify: Option<&ClaudeNotify>,
) -> Result<Value, ToolError> {
    let notifications = match notify {
        Some(notify) => notify
            .status()
            .await
            .map_err(|e| ToolError::execute_from(&e))?,
        None => Value::Null,
    };
    let mut value = match events {
        Some(events) => {
            let mut status = events
                .status()
                .await
                .map_err(|e| ToolError::execute_from(&e))?;
            status.insert("enabled".to_owned(), Value::Bool(true));
            Value::Object(status)
        }
        None => json!({
            "enabled": false,
            "checkedAt": null,
            "requests": [],
            "subscriptionTotal": 0,
            "subscriptions": [],
            "deliveries": {
                "pending": 0,
                "withheld": 0,
                "delivered": 0,
                "failed": 0,
                "recent": [],
            },
        }),
    };
    if let Some(map) = value.as_object_mut() {
        map.insert("claudeNotifications".to_owned(), notifications);
    }
    conform(&defs::EVENTS_STATUS, value)
}

pub fn event_tools(
    events: Option<McpEventService>,
    notify: Option<ClaudeNotify>,
) -> Result<Vec<McpTool>, ToolMetaError> {
    let tool = typed_tool(
        &defs::EVENTS_STATUS,
        move |_: EmptyInput, _: ToolContext| {
            let events = events.clone();
            let notify = notify.clone();
            async move { events_status(events.as_ref(), notify.as_ref()).await }
        },
    )?;
    Ok(vec![tool])
}
