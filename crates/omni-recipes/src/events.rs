//! MCP Events publications (`docs/mcp-events.md`): `recipes.inbox_added` and
//! `recipes.cook_finished`. Both are published after the transaction that
//! made them true has committed, keyed by the item or session, so a replay
//! is dropped by the outbox.

use omni_api::events::{
    RECIPES_COOK_FINISHED, RECIPES_INBOX_ADDED, RecipesCookFinished, RecipesInboxAdded,
    bounded_title,
};
use omni_runtime::ports::{EventPublication, Ports};
use serde_json::Value;

const LOG: &str = "Recipes";

fn publication<T: serde::Serialize>(
    name: &'static str,
    dedup_key: String,
    occurred_at_ms: i64,
    payload: &T,
) -> Option<EventPublication> {
    let Ok(Value::Object(data)) = serde_json::to_value(payload) else {
        return None;
    };
    Some(EventPublication {
        name,
        dedup_key,
        occurred_at_ms,
        data,
    })
}

/// A captured item became visible to agents.
pub fn inbox_added(
    item_id: &str,
    kind: &str,
    title: Option<&str>,
    at: i64,
) -> Option<EventPublication> {
    publication(
        RECIPES_INBOX_ADDED,
        format!("inbox:{item_id}"),
        at,
        &RecipesInboxAdded {
            item_id: item_id.to_owned(),
            kind: kind.to_owned(),
            title: title.and_then(bounded_title),
        },
    )
}

/// A cook session was finished and counted as a cook.
pub fn cook_finished(
    session_id: &str,
    recipe_id: &str,
    title: &str,
    open_adjustments: u32,
    at: i64,
) -> Option<EventPublication> {
    publication(
        RECIPES_COOK_FINISHED,
        format!("cook:{session_id}"),
        at,
        &RecipesCookFinished {
            session_id: session_id.to_owned(),
            recipe_id: recipe_id.to_owned(),
            title: bounded_title(title),
            open_adjustments,
        },
    )
}

/// Publishes when MCP Events are enabled. A failure is logged and never
/// changes the outcome of the write it follows.
pub async fn publish(ports: &Ports, event: Option<EventPublication>) {
    let Some(event) = event else {
        return;
    };
    if let Err(error) = ports.publish_event(&event).await {
        tracing::warn!(target: LOG, error = %error, "Recipe event not published");
    }
}
