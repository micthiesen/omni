//! `notify_owner_blocked` and `notify_owner_requested`.

pub mod defs;

use omni_core::js::to_iso_string;
use omni_mcp_kit::{McpTool, ToolContext, ToolError, ToolMetaError, typed_tool};

use self::defs::{
    NotifyOwnerBlockedInput, NotifyOwnerOutput, NotifyOwnerRequestedInput, NotifyStatus,
};
use crate::owner_notify::{
    BlockedFields, DeliveryState, NotificationKind, NotifyOutcome, OwnerNotifier, OwnerNotifyError,
    OwnerPush, clean_line, compose_blocked, compose_requested, https_url,
};

fn tool_error(error: OwnerNotifyError) -> ToolError {
    match error {
        OwnerNotifyError::Input(message) => ToolError::input(message),
        OwnerNotifyError::Store(error) => ToolError::execute_from(&error),
        other => ToolError::execute(other.to_string()),
    }
}

fn link(
    url: Option<&str>,
    url_title: Option<&str>,
) -> Result<(Option<String>, Option<String>), OwnerNotifyError> {
    let url = url.map(https_url).transpose()?;
    let url_title = match (&url, url_title) {
        (Some(_), Some(title)) => Some(clean_line("urlTitle", title)?),
        (None, Some(_)) => {
            return Err(OwnerNotifyError::Input("urlTitle needs a url".to_owned()));
        }
        _ => None,
    };
    Ok((url, url_title))
}

fn output(outcome: NotifyOutcome) -> NotifyOwnerOutput {
    let (status, note) = match (outcome.state, outcome.replayed) {
        (DeliveryState::Sent, false) if outcome.kind == NotificationKind::Blocked => (
            NotifyStatus::Sent,
            "Delivered. Do not notify again for this blocker; poll for the result.",
        ),
        (DeliveryState::Sent, false) => (NotifyStatus::Sent, "Delivered."),
        (DeliveryState::Sent, true) => (
            NotifyStatus::Sent,
            "Already delivered by an earlier call with this key; nothing was resent.",
        ),
        (DeliveryState::Recorded, _) => (
            NotifyStatus::Recorded,
            "Recorded without sending (preview or test mode); nothing reached the owner.",
        ),
        (DeliveryState::Uncertain, _) => (
            NotifyStatus::Uncertain,
            "The push may or may not have arrived and is never resent automatically. Do not send another notification for this; continue and poll for the result.",
        ),
    };
    NotifyOwnerOutput {
        status,
        replayed: outcome.replayed,
        kind: outcome.kind,
        idempotency_key: outcome.key,
        reserved_at: to_iso_string(outcome.reserved_at),
        sent_at: outcome.sent_at.map(to_iso_string),
        remaining_this_hour: outcome.remaining_this_hour as u64,
        remaining_today: outcome.remaining_today as u64,
        note: note.to_owned(),
    }
}

async fn blocked(
    notifier: OwnerNotifier,
    input: NotifyOwnerBlockedInput,
) -> Result<NotifyOwnerOutput, OwnerNotifyError> {
    let (title, message) = compose_blocked(&BlockedFields {
        task: &input.task,
        blocker: &input.blocker,
        steps: &input.steps,
        from_phone: input.from_phone,
        meanwhile: input.meanwhile.as_deref(),
    })?;
    let (url, url_title) = link(input.url.as_deref(), input.url_title.as_deref())?;
    let push = OwnerPush {
        kind: NotificationKind::Blocked,
        title,
        message,
        url,
        url_title,
        priority: input.priority.unwrap_or_default(),
    };
    notifier
        .notify(input.idempotency_key, push)
        .await
        .map(output)
}

async fn requested(
    notifier: OwnerNotifier,
    input: NotifyOwnerRequestedInput,
) -> Result<NotifyOwnerOutput, OwnerNotifyError> {
    let (title, message) = compose_requested(&input.request, &input.title, &input.message)?;
    let (url, url_title) = link(input.url.as_deref(), input.url_title.as_deref())?;
    let push = OwnerPush {
        kind: NotificationKind::Requested,
        title,
        message,
        url,
        url_title,
        priority: input.priority.unwrap_or_default(),
    };
    notifier
        .notify(input.idempotency_key, push)
        .await
        .map(output)
}

/// Builds the owner-notification tools over one shared ledger.
pub fn tools(notifier: OwnerNotifier) -> Result<Vec<McpTool>, ToolMetaError> {
    let for_blocked = notifier.clone();
    let blocked_tool = typed_tool(
        &defs::NOTIFY_OWNER_BLOCKED,
        move |input: NotifyOwnerBlockedInput, _cx: ToolContext| {
            let notifier = for_blocked.clone();
            async move { blocked(notifier, input).await.map_err(tool_error) }
        },
    )?;
    let requested_tool = typed_tool(
        &defs::NOTIFY_OWNER_REQUESTED,
        move |input: NotifyOwnerRequestedInput, _cx: ToolContext| {
            let notifier = notifier.clone();
            async move { requested(notifier, input).await.map_err(tool_error) }
        },
    )?;
    Ok(vec![blocked_tool, requested_tool])
}
