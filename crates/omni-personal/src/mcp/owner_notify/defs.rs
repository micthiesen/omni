//! The owner-notification tools' contracts: metadata, policy and the schema
//! types their input and output schemas derive from (`omni_mcp_kit::schema`).
//! The handlers decode and encode with these same types. After changing
//! anything here run `cargo xtask mcp-golden` and review the snapshot diff.

use omni_mcp_kit::{Annotations, ApprovalPolicy, Policy, ToolDef, ToolDefinition, ToolInfo};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::owner_notify::{NotificationKind, NotificationPriority};

const ANNOTATIONS: Annotations = Annotations {
    read_only_hint: false,
    destructive_hint: false,
    idempotent_hint: true,
    open_world_hint: true,
};

const POLICY: Policy = Policy {
    side_effects: &[
        "Sends one push notification to Michael's phone (Omni app, or Pushover when the app is not set up)",
    ],
    cost: "No monetary cost; one push. Shares a limit of 10 per hour and 40 per day",
    recommended: ApprovalPolicy::Allow,
};

pub static NOTIFY_OWNER_BLOCKED: ToolDef<NotifyOwnerBlockedInput, NotifyOwnerOutput> = ToolDef::new(
    ToolInfo {
        name: "notify_owner_blocked",
        title: "Notify Michael of a Blocker",
        description: "Send Michael a push notification when your work is blocked on something only he can do and you cannot reasonably continue without it: an approval, a sign-in or two-factor prompt, a credential he must enter himself, a payment, a physical action, or a product decision with no safe default. Use it rarely. First finish all independent work and try every fix you can make yourself; a question you can answer with a sensible default is not a blocker. He is usually away from his computer when this matters, so prefer asks he can clear from a phone and say plainly whether a computer is needed. Give the exact steps, link the place to act when there is one, send one notification per blocker, then poll for the result instead of notifying again. Never use it for progress updates, FYI, or routine completion, and never include secrets, tokens or codes. Shares a limit of 10 per hour and 40 per day with notify_owner_requested; repeating a call with the same idempotencyKey returns the first result without sending again.",
        annotations: ANNOTATIONS,
        policy: POLICY,
    },
);

pub static NOTIFY_OWNER_REQUESTED: ToolDef<NotifyOwnerRequestedInput, NotifyOwnerOutput> =
    ToolDef::new(ToolInfo {
        name: "notify_owner_requested",
        title: "Notify Michael as Requested",
        description: "Send Michael a push notification because he explicitly asked to be told about something, such as \"ping me when it's done\" or \"let me know if the deploy fails\". Put his request, quoted or closely paraphrased, in request. Do not use it for unrequested progress, FYI or completion notices; if you are stuck on something only he can do, use notify_owner_blocked instead. Keep it short: the outcome, and anything he needs to know or do next. Never include secrets. Shares a limit of 10 per hour and 40 per day with notify_owner_blocked; repeating a call with the same idempotencyKey returns the first result without sending again.",
        annotations: ANNOTATIONS,
        policy: POLICY,
    });

/// Every owner-notification tool, in serving order.
pub static TOOLS: [&dyn ToolDefinition; 2] = [&NOTIFY_OWNER_BLOCKED, &NOTIFY_OWNER_REQUESTED];

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NotifyOwnerBlockedInput {
    /// A unique key per notification (16-128 of A-Z a-z 0-9 _ -). Reuse it
    /// only to retry the same call.
    #[schemars(pattern(r"^[A-Za-z0-9_-]{16,128}$"))]
    pub idempotency_key: String,
    /// The blocked work as a short name, e.g. "Omni MCP rollout". The title
    /// becomes "Blocked: <task>".
    #[schemars(length(min = 1, max = 60))]
    pub task: String,
    /// What is blocked and why only Michael can clear it, in one or two plain
    /// sentences.
    #[schemars(length(min = 1, max = 240))]
    pub blocker: String,
    /// The exact actions for Michael, in order; each becomes a numbered line.
    /// Name the app, page or command and the value to choose or enter.
    #[schemars(length(min = 1, max = 4), inner(length(min = 1, max = 140)))]
    pub steps: Vec<String>,
    /// Whether every step can be done from a phone. False tells him he needs a
    /// computer.
    pub from_phone: bool,
    /// What you will do while waiting, e.g. "Continuing with tests; checking
    /// every 10 minutes".
    #[schemars(length(min = 1, max = 150))]
    pub meanwhile: Option<String>,
    /// An https link that opens the place to act, such as an approval page or
    /// pull request.
    #[schemars(length(max = 512), pattern(r"^https://"))]
    pub url: Option<String>,
    /// The link's button label; "Open" by default.
    #[schemars(length(min = 1, max = 50))]
    pub url_title: Option<String>,
    /// "high" breaks through quiet hours; use it only when waiting would lose
    /// work, money or a deadline.
    #[schemars(extend("default" = "normal"))]
    pub priority: Option<NotificationPriority>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NotifyOwnerRequestedInput {
    /// A unique key per notification (16-128 of A-Z a-z 0-9 _ -). Reuse it
    /// only to retry the same call.
    #[schemars(pattern(r"^[A-Za-z0-9_-]{16,128}$"))]
    pub idempotency_key: String,
    /// Michael's request this answers, quoted or closely paraphrased, e.g.
    /// "ping me when the import finishes".
    #[schemars(length(min = 1, max = 160))]
    pub request: String,
    /// The headline, e.g. "Import finished" or "Deploy failed".
    #[schemars(length(min = 1, max = 80))]
    pub title: String,
    /// The outcome and anything he needs to know or do next, in plain text.
    #[schemars(length(min = 1, max = 600))]
    pub message: String,
    /// An https link to the result, such as a pull request or page.
    #[schemars(length(max = 512), pattern(r"^https://"))]
    pub url: Option<String>,
    /// The link's button label; "Open" by default.
    #[schemars(length(min = 1, max = 50))]
    pub url_title: Option<String>,
    /// "high" breaks through quiet hours; use it only when he asked to be told
    /// urgently.
    #[schemars(extend("default" = "normal"))]
    pub priority: Option<NotificationPriority>,
}

#[derive(Clone, Copy, Debug, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum NotifyStatus {
    Sent,
    Recorded,
    Uncertain,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NotifyOwnerOutput {
    /// "sent": delivered to the app or Pushover. "recorded": captured without
    /// sending (preview or test mode). "uncertain": the push may or may not
    /// have arrived; it is never resent.
    pub status: NotifyStatus,
    /// True when this idempotencyKey was already used and nothing new was sent.
    pub replayed: bool,
    /// Which tool sent it: "blocked" or "requested".
    pub kind: NotificationKind,
    pub idempotency_key: String,
    pub reserved_at: String,
    pub sent_at: Option<String>,
    /// Notifications left in the rolling hour, across both tools.
    pub remaining_this_hour: u64,
    /// Notifications left in the rolling 24 hours, across both tools.
    pub remaining_today: u64,
    /// What to do next.
    pub note: String,
}
