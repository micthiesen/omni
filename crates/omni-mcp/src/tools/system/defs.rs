//! The system tools' contracts: metadata, policy and the schema types
//! their input and output schemas derive from (`omni_mcp_kit::schema`). These
//! types describe the wire format only; the handlers decode and encode with
//! their own types. After changing anything here run `cargo xtask mcp-golden`
//! and review the snapshot diff.

use std::collections::BTreeMap;

use omni_mcp_kit::schema::{Lit, Literal, lead_description, nullable};
use omni_mcp_kit::{Annotations, ApprovalPolicy, Policy, ToolDef, ToolDefinition, ToolInfo};
use schemars::JsonSchema;
use serde_json::{Map, Value};

pub static SYSTEM_STATUS: ToolDef<SystemStatusInput, SystemStatusOutput> = ToolDef::new(ToolInfo {
    name: "system_status",
    title: "System Status",
    description: "Report which high-level Omni capabilities are configured without exposing account identifiers, credentials, configuration values, or host details.",
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

pub static TASKS_LIST: ToolDef<TasksListInput, TasksListOutput> = ToolDef::new(ToolInfo {
    name: "tasks_list",
    title: "List Tasks",
    description: "List registered Omni tasks with schedules, running state, upcoming executions, and the latest recorded run.",
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

pub static TASK_RUN: ToolDef<TaskRunInput, TaskRunOutput> = ToolDef::new(ToolInfo {
    name: "task_run",
    title: "Run Task",
    description: "Queue one registered task for immediate execution. The task may send notifications, call paid services, modify external systems, or perform other consequential work; inspect the task and obtain approval first.",
    annotations: Annotations {
        read_only_hint: false,
        destructive_hint: false,
        idempotent_hint: false,
        open_world_hint: true,
    },
    policy: Policy {
        side_effects: &[
            "Queues task execution",
            "Effects depend on the selected task and may include external communications or external mutations",
        ],
        cost: "task-dependent; some tasks invoke paid AI, search, notification, or media services",
        recommended: ApprovalPolicy::RequireApproval,
    },
});

pub static TASK_RUNS_LIST: ToolDef<TaskRunsListInput, TaskRunsListOutput> = ToolDef::new(
    ToolInfo {
        name: "task_runs_list",
        title: "List Task Runs",
        description: "List recent persisted task runs, optionally filtered by exact task name. Results are newest first and bounded to the newest 500 runs.",
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
    },
);

pub static TASK_RUN_GET: ToolDef<TaskRunGetInput, TaskRunGetOutput> = ToolDef::new(ToolInfo {
    name: "task_run_get",
    title: "Get Task Run",
    description: "Get one task run and a bounded page of its captured logs. Log messages are truncated to prevent oversized results.",
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

pub static LIVESTREAMS_LIST: ToolDef<LivestreamsListInput, LivestreamsListOutput> = ToolDef::new(
    ToolInfo {
        name: "livestreams_list",
        title: "List Livestreams",
        description: "List configured streamer identities and their current persisted live state without polling external platforms.",
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
    },
);

pub static LIVESTREAM_GET: ToolDef<LivestreamGetInput, LivestreamGetOutput> = ToolDef::new(
    ToolInfo {
        name: "livestream_get",
        title: "Get Livestream",
        description: "Get one streamer's persisted live state, with optional bounded viewer metrics, completed sessions, and intelligence diagnostics. This does not poll the platforms.",
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
    },
);

/// Every system tool, in serving order.
pub static TOOLS: [&dyn ToolDefinition; 7] = [
    &SYSTEM_STATUS,
    &TASKS_LIST,
    &TASK_RUN,
    &TASK_RUNS_LIST,
    &TASK_RUN_GET,
    &LIVESTREAMS_LIST,
    &LIVESTREAM_GET,
];

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct SystemStatusInput {}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct Capabilities {
    pub task_controls: bool,
    pub livestreams: bool,
    pub livestream_intelligence: bool,
    pub i_cloud_email: bool,
    pub i_cloud_calendar: bool,
    pub web_search: bool,
    pub ios_controls: bool,
    pub printing: bool,
}

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct SystemStatusOutput {
    pub capabilities: Capabilities,
}

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct TasksListInput {
    #[schemars(description = "Zero-based result offset", extend("default" = 0))]
    pub cursor: Option<u64>,
    #[schemars(range(min = 1, max = 100), extend("default" = 25))]
    pub limit: Option<u64>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "lowercase")]
pub enum Trigger {
    Schedule,
    Manual,
    Startup,
    Catchup,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "lowercase")]
pub enum Status {
    Running,
    Success,
    Error,
    Degraded,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct LastRun {
    pub run_id: String,
    pub task_name: String,
    pub trigger: Trigger,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub scheduled_for: Option<f64>,
    pub started_at: f64,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<f64>,
    pub status: Status,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct Task {
    pub name: String,
    pub display_name: Option<String>,
    pub schedule: String,
    pub running: bool,
    pub next_runs: Vec<String>,
    pub last_run: Option<LastRun>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct TasksListOutput {
    pub tasks: Vec<Task>,
    pub next_cursor: Option<u64>,
    pub total: u64,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskRunInput {
    #[schemars(length(min = 1, max = 200))]
    pub task_name: String,
    #[schemars(description = "Optional task-specific manual input", transform = lead_description)]
    pub input: Option<Map<String, Value>>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskRunOutput {
    pub run_id: String,
    pub task_name: String,
    #[schemars(transform = Literal(true))]
    pub queued: Lit,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskRunsListInput {
    #[schemars(length(min = 1, max = 200))]
    pub task_name: Option<String>,
    #[schemars(range(max = 499), extend("default" = 0))]
    pub cursor: Option<u64>,
    #[schemars(range(min = 1, max = 100), extend("default" = 25))]
    pub limit: Option<u64>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskRunsListOutput {
    pub runs: Vec<LastRun>,
    pub next_cursor: Option<u64>,
    pub total: u64,
    pub result_window_truncated: bool,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskRunGetInput {
    #[schemars(length(min = 1, max = 300))]
    pub run_id: String,
    #[schemars(range(max = 19999), extend("default" = 0))]
    pub log_cursor: Option<u64>,
    #[schemars(range(min = 1, max = 200), extend("default" = 100))]
    pub log_limit: Option<u64>,
    #[schemars(range(min = 100, max = 4000), extend("default" = 2000))]
    pub max_message_chars: Option<u64>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct Log {
    pub timestamp: f64,
    pub level: String,
    pub logger: String,
    pub message: String,
    pub message_truncated: bool,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskRunGetOutput {
    pub run: LastRun,
    pub logs: Vec<Log>,
    pub log_next_cursor: Option<u64>,
    pub log_total: u64,
    pub dropped_logs: u64,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct LivestreamsListInput {
    #[schemars(extend("default" = false))]
    pub live_only: Option<bool>,
    #[schemars(description = "Zero-based result offset", extend("default" = 0))]
    pub cursor: Option<u64>,
    #[schemars(range(min = 1, max = 100), extend("default" = 25))]
    pub limit: Option<u64>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "lowercase")]
pub enum Tier {
    Primary,
    Background,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "lowercase")]
pub enum Platform {
    Youtube,
    Twitch,
    Kick,
}

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct Binding {
    pub platform: Platform,
    pub username: String,
    #[schemars(url)]
    pub url: String,
}

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct Dgg {
    pub hosted: bool,
    pub viewers: Option<f64>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct Source {
    pub platform: Platform,
    pub username: String,
    pub title: String,
    pub viewer_count: Option<f64>,
    pub category: Option<String>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct Livestream {
    pub id: String,
    pub display_name: String,
    pub tier: Tier,
    pub bindings: Vec<Binding>,
    pub dgg: Option<Dgg>,
    pub live: bool,
    pub title: Option<String>,
    pub category: Option<String>,
    pub viewer_count: Option<f64>,
    pub max_viewer_count: Option<f64>,
    pub started_at: Option<f64>,
    pub last_started_at: Option<f64>,
    pub last_ended_at: Option<f64>,
    pub primary: Option<Binding>,
    pub sources: Vec<Source>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct LivestreamsListOutput {
    pub livestreams: Vec<Livestream>,
    pub next_cursor: Option<u64>,
    pub total: u64,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "lowercase")]
pub enum IncludeItem {
    Metrics,
    Sessions,
    Intelligence,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct LivestreamGetInput {
    #[schemars(length(min = 1, max = 200))]
    pub streamer_id: String,
    #[schemars(length(max = 3), extend("default" = []))]
    pub include: Option<Vec<IncludeItem>>,
    #[schemars(range(min = 1, max = 180), extend("default" = 30))]
    pub metrics_days: Option<u64>,
    #[schemars(range(min = 1, max = 100), extend("default" = 20))]
    pub session_limit: Option<u64>,
    #[schemars(range(min = 1, max = 100), extend("default" = 25))]
    pub intelligence_event_limit: Option<u64>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct DailyBucket {
    pub date: String,
    pub max_viewers: u64,
    pub timestamp: f64,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct MetricsPlatform {
    pub platform: String,
    pub username: String,
    pub daily_buckets: Vec<DailyBucket>,
    pub all_time_max: u64,
    pub all_time_max_timestamp: f64,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct Metrics {
    pub daily_buckets: Vec<DailyBucket>,
    pub all_time_max: u64,
    pub all_time_max_timestamp: f64,
    pub platforms: Vec<MetricsPlatform>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct Session {
    pub started_at: f64,
    pub ended_at: f64,
    #[schemars(range(min = 0))]
    pub duration_ms: f64,
    pub peak_viewers: u64,
    pub title: String,
    pub platform: Platform,
    pub username: String,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "lowercase")]
pub enum ContentKind {
    Politics,
    Debate,
    News,
    Gaming,
    Conversation,
    Other,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct Semantic {
    pub headline: String,
    pub topics: Vec<String>,
    pub content_kind: ContentKind,
    pub importance: f64,
    pub reason: String,
    pub updated_at: f64,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct Trend {
    pub percent_change: f64,
    pub viewers_per_minute: f64,
    pub dgg_percent_change: Option<f64>,
    pub anomalous: bool,
    pub reason: Option<String>,
    #[schemars(skip_serializing_if = "Option::is_none", transform = nullable)]
    pub current_viewers: Option<f64>,
    #[schemars(skip_serializing_if = "Option::is_none", transform = nullable)]
    pub baseline_viewers: Option<f64>,
    #[schemars(skip_serializing_if = "Option::is_none", transform = nullable)]
    pub current_dgg_viewers: Option<f64>,
    #[schemars(skip_serializing_if = "Option::is_none", transform = nullable)]
    pub baseline_dgg_viewers: Option<f64>,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub baseline_samples: Option<f64>,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub candidate_observations: Option<f64>,
    #[schemars(skip_serializing_if = "Option::is_none", transform = nullable)]
    pub suppression_reason: Option<String>,
    pub updated_at: f64,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct Summary {
    pub text: String,
    pub topic: String,
    pub confidence: f64,
    pub transcript_excerpt: String,
    pub updated_at: f64,
    pub window_seconds: f64,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct Chapter {
    pub chapter_id: String,
    pub started_at: f64,
    pub title: String,
    pub summary: String,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "lowercase")]
pub enum State {
    Possible,
    Confirmed,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct DestinyPresence {
    pub state: State,
    pub confidence: f64,
    pub detected_at: f64,
    pub reason: String,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "snake_case")]
pub enum Type {
    DestinyGuest,
    BreakingNews,
    Debate,
    GuestJoined,
    MajorAnnouncement,
    ViewerSurge,
    CrossStreamTopic,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct LatestAlert {
    pub alert_id: String,
    pub r#type: Type,
    pub title: String,
    pub message: String,
    pub reason: String,
    pub confidence: f64,
    pub created_at: f64,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct Current {
    pub streamer_id: String,
    pub session_started_at: f64,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub semantic: Option<Semantic>,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub trend: Option<Trend>,
    pub relevance_score: f64,
    pub relevance_reasons: Vec<String>,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub summary: Option<Summary>,
    pub chapters: Vec<Chapter>,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub destiny_presence: Option<DestinyPresence>,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub latest_alert: Option<LatestAlert>,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub alerted_at_by_type: Option<BTreeMap<String, f64>>,
    pub updated_at: f64,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "lowercase")]
pub enum MetadataStatus {
    Idle,
    Running,
    Success,
    Skipped,
    Error,
}

#[derive(JsonSchema)]
#[schemars(untagged)]
pub enum MetricsValue {
    Text(String),
    Number(f64),
    Bool(bool),
    Null,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct Metadata {
    pub status: MetadataStatus,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub eligible: Option<bool>,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<f64>,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<f64>,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub next_at: Option<f64>,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<f64>,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub metrics: Option<BTreeMap<String, MetricsValue>>,
}

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct Stages {
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub voice: Option<Metadata>,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub summary: Option<Metadata>,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub alert: Option<Metadata>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct Diagnostics {
    pub streamer_id: String,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub session_started_at: Option<f64>,
    pub stages: Stages,
    pub updated_at: f64,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "lowercase")]
pub enum Kind {
    Session,
    Metadata,
    Voice,
    Summary,
    Alert,
    Feedback,
    Anomaly,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "lowercase")]
pub enum EventStatus {
    Info,
    Success,
    Warning,
    Error,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct Event {
    pub event_id: String,
    pub streamer_id: String,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub session_started_at: Option<f64>,
    pub created_at: f64,
    pub kind: Kind,
    pub status: EventStatus,
    pub title: String,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<f64>,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub cost_cents: Option<f64>,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub metrics: Option<BTreeMap<String, MetricsValue>>,
}

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct Capture {
    pub running: f64,
    pub queued: f64,
}

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct Queues {
    pub capture: Capture,
    pub speech: Capture,
    pub llm: Capture,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct Budget {
    pub spent_cents: f64,
    pub limit_cents: f64,
    pub remaining_cents: f64,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct Intervals {
    pub voice_seconds: f64,
    pub summary_seconds: f64,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct Runtime {
    #[schemars(transform = Literal(true))]
    pub enabled: Lit,
    pub voiceprint_loaded: bool,
    pub model: String,
    pub queues: Queues,
    pub active_stream_count: u64,
    pub active_voice_target_count: u64,
    pub budget: Budget,
    pub intervals: Intervals,
}

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct Intelligence {
    pub current: Option<Current>,
    pub diagnostics: Option<Diagnostics>,
    pub events: Vec<Event>,
    pub runtime: Option<Runtime>,
}

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct LivestreamGetOutput {
    pub livestream: Livestream,
    pub metrics: Option<Metrics>,
    pub sessions: Option<Vec<Session>>,
    pub intelligence: Option<Intelligence>,
}
