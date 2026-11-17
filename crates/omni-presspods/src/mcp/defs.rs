//! The PressPods tools' contracts: metadata, policy and the schema types
//! their input and output schemas derive from (`omni_mcp_kit::schema`). These
//! types describe the wire format only; the handlers decode and encode with
//! their own types. After changing anything here run `cargo xtask mcp-golden`
//! and review the snapshot diff.

use omni_mcp_kit::schema::{Lit, Literal, one_of};
use omni_mcp_kit::{Annotations, ApprovalPolicy, Policy, ToolDef, ToolDefinition, ToolInfo};
use schemars::JsonSchema;

pub static PRESSPODS_LIST: ToolDef<PresspodsListInput, PresspodsListOutput> = ToolDef::new(
    ToolInfo {
        name: "presspods_list",
        title: "List PressPods Resources",
        description: "List persisted PressPods episodes, queued, processing, and failed jobs, or the attempt history. `history` is the durable record of every failed attempt (retried or final) and of each success that followed one, newest first, kept 180 days; it survives deleting or resubmitting the job. Each item has the URL, domain, attempt number, error, failing step and every retriever's outcome (error class, HTTP status, extracted length, rating); `resolvedAt` is set on a failure once the same article later produced an episode. Filter history with `query` (URL text), `domain`, `since` and `until`. Audio filenames, bytes and article text are not exposed.",
        annotations: Annotations {
            read_only_hint: true,
            destructive_hint: false,
            idempotent_hint: true,
            open_world_hint: false,
        },
        policy: Policy {
            side_effects: &[],
            cost: "No external traffic or monetary cost",
            recommended: ApprovalPolicy::Allow,
        },
    },
);

pub static PRESSPODS_EPISODE_GET: ToolDef<PresspodsEpisodeGetInput, PresspodsEpisodeGetOutput> =
    ToolDef::new(ToolInfo {
        name: "presspods_episode_get",
        title: "Get PressPods Episode",
        description: "Get compact metadata for one PressPods episode. Use presspods_transcript_read for bounded narration text.",
        annotations: Annotations {
            read_only_hint: true,
            destructive_hint: false,
            idempotent_hint: true,
            open_world_hint: false,
        },
        policy: Policy {
            side_effects: &[],
            cost: "No external traffic or monetary cost",
            recommended: ApprovalPolicy::Allow,
        },
    });

pub static PRESSPODS_TRANSCRIPT_READ: ToolDef<
    PresspodsTranscriptReadInput,
    PresspodsTranscriptReadOutput,
> = ToolDef::new(ToolInfo {
    name: "presspods_transcript_read",
    title: "Read PressPods Transcript",
    description: "Read a bounded page of an episode's cleaned narration transcript. This never returns audio bytes or filesystem paths.",
    annotations: Annotations {
        read_only_hint: true,
        destructive_hint: false,
        idempotent_hint: true,
        open_world_hint: false,
    },
    policy: Policy {
        side_effects: &[],
        cost: "No external traffic or monetary cost",
        recommended: ApprovalPolicy::Allow,
    },
});

pub static PRESSPODS_SUBMIT: ToolDef<PresspodsSubmitInput, PresspodsSubmitOutput> = ToolDef::new(
    ToolInfo {
        name: "presspods_submit",
        title: "Submit PressPods Episode",
        description: "Queue a public article URL for retrieval, model cleaning, TTS synthesis, podcast publication, optional Karakeep bookmarking, and notification. This has material compute/model cost and external effects, so the owner's approval is required.",
        annotations: Annotations {
            read_only_hint: false,
            destructive_hint: false,
            idempotent_hint: false,
            open_world_hint: true,
        },
        policy: Policy {
            side_effects: &[
                "Queues paid or self-hosted model and TTS work",
                "May bookmark the URL in Karakeep",
                "Publishes an episode to the personal feed and sends a notification after processing",
            ],
            cost: "May incur metadata/cleaning model and TTS charges; ElevenLabs is approximately $0.10 per 1,000 characters when configured",
            recommended: ApprovalPolicy::RequireApproval,
        },
    },
);

pub static PRESSPODS_RETRY: ToolDef<PresspodsRetryInput, PresspodsRetryOutput> = ToolDef::new(
    ToolInfo {
        name: "presspods_retry",
        title: "Retry PressPods Work",
        description: "Regenerate an existing episode or retry a failed job. This starts model/TTS work and can replace a published episode, so the owner's approval is required.",
        annotations: Annotations {
            read_only_hint: false,
            destructive_hint: false,
            idempotent_hint: false,
            open_world_hint: true,
        },
        policy: Policy {
            side_effects: &[
                "Queues paid or self-hosted model and TTS work",
                "May replace an existing published episode and send a notification",
            ],
            cost: "May incur metadata/cleaning model and TTS charges",
            recommended: ApprovalPolicy::RequireApproval,
        },
    },
);

pub static PRESSPODS_DELETE: ToolDef<PresspodsDeleteInput, PresspodsDeleteOutput> = ToolDef::new(
    ToolInfo {
        name: "presspods_delete",
        title: "Delete PressPods Resource",
        description: "Permanently delete an episode and its audio, or dismiss a non-processing job and its resume checkpoints. This is destructive and requires the owner's approval.",
        annotations: Annotations {
            read_only_hint: false,
            destructive_hint: true,
            idempotent_hint: true,
            open_world_hint: false,
        },
        policy: Policy {
            side_effects: &[
                "Permanently removes local episode/audio data or a queued/failed job and its checkpoints",
            ],
            cost: "No monetary cost",
            recommended: ApprovalPolicy::RequireApproval,
        },
    },
);

/// Every PressPods tool, in serving order.
pub static TOOLS: [&dyn ToolDefinition; 6] = [
    &PRESSPODS_LIST,
    &PRESSPODS_EPISODE_GET,
    &PRESSPODS_TRANSCRIPT_READ,
    &PRESSPODS_SUBMIT,
    &PRESSPODS_RETRY,
    &PRESSPODS_DELETE,
];

#[derive(JsonSchema)]
#[schemars(rename_all = "lowercase")]
pub enum Resource {
    Episodes,
    Jobs,
    History,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "lowercase")]
pub enum Status {
    Queued,
    Processing,
    Failed,
}

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct PresspodsListInput {
    pub resource: Resource,
    /// Jobs only.
    pub status: Option<Status>,
    /// Episodes: title, author, publication or domain text. Jobs and
    /// history: URL text.
    #[schemars(length(max = 300))]
    pub query: Option<String>,
    /// History only: the article's site, e.g. `theatlantic.com` (a full URL
    /// is reduced to its host); subdomains match and a leading `www.` is
    /// ignored.
    #[schemars(length(max = 2000))]
    pub domain: Option<String>,
    /// History only: first day to include, in the server's time zone.
    #[schemars(pattern(r"^\d{4}-\d{2}-\d{2}$"))]
    pub since: Option<String>,
    /// History only: last day to include, in the server's time zone.
    #[schemars(pattern(r"^\d{4}-\d{2}-\d{2}$"))]
    pub until: Option<String>,
    #[schemars(description = "Zero-based result offset", extend("default" = 0))]
    pub cursor: Option<u64>,
    #[schemars(range(min = 1, max = 100), extend("default" = 25))]
    pub limit: Option<u64>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct PresspodsListOutputEpisodesItem {
    pub episode_id: String,
    pub title: String,
    pub author: Option<String>,
    pub publication: Option<String>,
    pub domain: Option<String>,
    #[schemars(url)]
    pub article_url: String,
    pub excerpt: Option<String>,
    pub voice_name: Option<String>,
    pub voice_provider: Option<String>,
    pub duration_seconds: Option<f64>,
    pub file_bytes: u64,
    pub retriever_name: Option<String>,
    pub cost_cents: Option<f64>,
    pub created_at: i64,
    pub published_at: Option<f64>,
    pub run_id: Option<String>,
    pub chapter_count: u64,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct PresspodsListOutputEpisodes {
    pub next_cursor: Option<u64>,
    pub total: u64,
    #[schemars(transform = Literal("episodes"))]
    pub resource: Lit,
    pub items: Vec<PresspodsListOutputEpisodesItem>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct PresspodsListOutputJobsItem {
    pub job_id: String,
    #[schemars(url)]
    pub url: String,
    pub status: Status,
    pub attempts: u64,
    pub next_attempt_at: Option<f64>,
    pub last_error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_run_id: Option<String>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct PresspodsListOutputJobs {
    pub next_cursor: Option<u64>,
    pub total: u64,
    #[schemars(transform = Literal("jobs"))]
    pub resource: Lit,
    pub items: Vec<PresspodsListOutputJobsItem>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "lowercase")]
pub enum AttemptOutcome {
    /// Failed and requeued with backoff.
    Retrying,
    /// Failed for good.
    Failed,
    /// Produced an episode after earlier failures for the same article.
    Succeeded,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "snake_case")]
pub enum RetrieverErrorClass {
    HttpStatus,
    Timeout,
    Network,
    /// Text was extracted but rated not an article.
    InvalidArticle,
    /// Text was extracted but the rating model call failed.
    RatingFailed,
    Other,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct PresspodsListOutputHistoryRetriever {
    pub name: String,
    /// The extraction was rated a valid article.
    pub ok: bool,
    /// This extraction was the one narrated.
    pub selected: bool,
    pub error_class: Option<RetrieverErrorClass>,
    pub http_status: Option<u16>,
    pub error: Option<String>,
    /// Length of the extracted text, when there was one.
    pub text_chars: Option<u64>,
    /// The 0-10 extraction rating of a valid article.
    pub content_rating: Option<f64>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct PresspodsListOutputHistoryItem {
    pub attempt_id: String,
    pub job_id: String,
    pub url: String,
    pub domain: Option<String>,
    /// Attempt number within the job's retry budget; a manual retry or
    /// resubmit starts again at 1.
    pub attempt: u64,
    /// Epoch milliseconds.
    pub at: i64,
    pub outcome: AttemptOutcome,
    pub error: Option<String>,
    /// The pipeline step that failed.
    pub operation: Option<String>,
    /// Null when the attempt ended before retrieval finished.
    pub retrievers: Option<Vec<PresspodsListOutputHistoryRetriever>>,
    pub run_id: Option<String>,
    /// The episode a succeeded attempt produced.
    pub episode_id: Option<String>,
    /// On a failure: when the same article next produced an episode. Null
    /// while unresolved.
    pub resolved_at: Option<i64>,
    pub resolved_by_episode_id: Option<String>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct PresspodsListOutputHistory {
    pub next_cursor: Option<u64>,
    pub total: u64,
    #[schemars(transform = Literal("history"))]
    pub resource: Lit,
    pub items: Vec<PresspodsListOutputHistoryItem>,
}

#[derive(JsonSchema)]
#[schemars(untagged, transform = one_of)]
pub enum PresspodsListOutput {
    Episodes(PresspodsListOutputEpisodes),
    Jobs(PresspodsListOutputJobs),
    History(PresspodsListOutputHistory),
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct PresspodsEpisodeGetInput {
    #[schemars(length(min = 1, max = 200))]
    pub episode_id: String,
}

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct PresspodsEpisodeGetOutput {
    pub episode: PresspodsListOutputEpisodesItem,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct PresspodsTranscriptReadInput {
    #[schemars(length(min = 1, max = 200))]
    pub episode_id: String,
    #[schemars(extend("default" = 0))]
    pub offset: Option<u64>,
    #[schemars(range(min = 1, max = 10000), extend("default" = 4000))]
    pub max_chars: Option<u64>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct PresspodsTranscriptReadOutput {
    pub episode_id: String,
    pub title: String,
    pub offset: u64,
    pub text: String,
    pub next_offset: Option<u64>,
    pub total_chars: u64,
    pub truncated: bool,
}

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct PresspodsSubmitInput {
    pub url: String,
}

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct PresspodsSubmitOutput {
    pub job: PresspodsListOutputJobsItem,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase")]
pub struct PresspodsRetryInputEpisode {
    #[schemars(transform = Literal("episode"))]
    pub resource: Lit,
    #[schemars(length(min = 1, max = 200))]
    pub episode_id: String,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase")]
pub struct PresspodsRetryInputJob {
    #[schemars(transform = Literal("job"))]
    pub resource: Lit,
    #[schemars(length(min = 1, max = 200))]
    pub job_id: String,
}

#[derive(JsonSchema)]
#[schemars(untagged, transform = one_of)]
pub enum PresspodsRetryInput {
    Episode(PresspodsRetryInputEpisode),
    Job(PresspodsRetryInputJob),
}

pub type PresspodsRetryOutput = PresspodsSubmitOutput;

pub type PresspodsDeleteInput = PresspodsRetryInput;

#[derive(JsonSchema)]
#[schemars(rename_all = "lowercase")]
pub enum PresspodsDeleteResource {
    Episode,
    Job,
}

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct PresspodsDeleteOutput {
    pub resource: PresspodsDeleteResource,
    #[schemars(transform = Literal(true))]
    pub deleted: Lit,
}
