//! The podcast tools' contracts: metadata, policy and the schema types
//! their input and output schemas derive from (`omni_mcp_kit::schema`). These
//! types describe the wire format only; the handlers decode and encode with
//! their own types. After changing anything here run `cargo xtask mcp-golden`
//! and review the snapshot diff.

use std::collections::BTreeMap;

use omni_mcp_kit::schema::{Lit, Literal, one_of, positive};
use omni_mcp_kit::{Annotations, ApprovalPolicy, Policy, ToolDef, ToolDefinition, ToolInfo};
use schemars::JsonSchema;

pub static PODCAST_ACCOUNT_LIST: ToolDef<PodcastAccountListInput, PodcastAccountListOutput> =
    ToolDef::new(ToolInfo {
        name: "podcast_account_list",
        title: "List Podcast Account Resources",
        description: "List bounded subscriptions, queue, inbox, or recent listening history from the configured podcast account. Unavailable account state is reported as an error.",
        annotations: Annotations {
            read_only_hint: true,
            destructive_hint: false,
            idempotent_hint: true,
            open_world_hint: true,
        },
        policy: Policy {
            side_effects: &["Reads the configured podcast account through its rate-limited client"],
            cost: "No expected monetary cost; consumes bounded private account API traffic",
            recommended: ApprovalPolicy::Allow,
        },
    });

pub static PODCAST_ACCOUNT_SEARCH: ToolDef<PodcastAccountSearchInput, PodcastAccountSearchOutput> =
    ToolDef::new(ToolInfo {
        name: "podcast_account_search",
        title: "Search Podcast Account",
        description: "Search shows or episodes through the configured podcast account client. Results are bounded and do not change subscriptions or queue state.",
        annotations: Annotations {
            read_only_hint: true,
            destructive_hint: false,
            idempotent_hint: true,
            open_world_hint: true,
        },
        policy: Policy {
            side_effects: &[
                "Searches the configured podcast account through its rate-limited client",
            ],
            cost: "No expected monetary cost; consumes bounded private account API traffic",
            recommended: ApprovalPolicy::Allow,
        },
    });

pub static PODCAST_ACCOUNT_UPDATE: ToolDef<PodcastAccountUpdateInput, PodcastAccountUpdateOutput> =
    ToolDef::new(ToolInfo {
        name: "podcast_account_update",
        title: "Update Podcast Account",
        description: "Enqueue or dequeue an episode, clear an Inbox item, or subscribe to a show. These change an external podcast account and require the owner's approval.",
        annotations: Annotations {
            read_only_hint: false,
            destructive_hint: true,
            idempotent_hint: true,
            open_world_hint: true,
        },
        policy: Policy {
            side_effects: &[
                "Changes queue, Inbox, or subscription state on an external podcast account",
            ],
            cost: "No expected monetary cost; consumes private account API traffic",
            recommended: ApprovalPolicy::RequireApproval,
        },
    });

pub static PODCAST_RECOMMENDATIONS_LIST: ToolDef<
    PodcastRecommendationsListInput,
    PodcastRecommendationsListOutput,
> = ToolDef::new(ToolInfo {
    name: "podcast_recommendations_list",
    title: "List Podcast Recommendations",
    description: "List persisted podcast episode recommendations and outcomes.",
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

pub static PODCAST_RECOMMENDATION_GET: ToolDef<
    PodcastRecommendationGetInput,
    PodcastRecommendationGetOutput,
> = ToolDef::new(ToolInfo {
    name: "podcast_recommendation_get",
    title: "Get Podcast Recommendation",
    description: "Get one persisted podcast recommendation by recommendation ID.",
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

pub static PODCAST_RECOMMENDATION_FEEDBACK: ToolDef<
    PodcastRecommendationFeedbackInput,
    PodcastRecommendationFeedbackOutput,
> = ToolDef::new(ToolInfo {
    name: "podcast_recommendation_feedback",
    title: "Record Podcast Recommendation Feedback",
    description: "Record good-pick or not-for-me feedback and/or a bounded note. This changes only Omni's local recommendation state.",
    annotations: Annotations {
        read_only_hint: false,
        destructive_hint: false,
        idempotent_hint: false,
        open_world_hint: false,
    },
    policy: Policy {
        side_effects: &["Updates local podcast feedback used by future taste analysis"],
        cost: "No external traffic or monetary cost",
        recommended: ApprovalPolicy::Allow,
    },
});

pub static PODCAST_RECOMMENDATION_RECORD: ToolDef<
    PodcastRecommendationRecordInput,
    PodcastRecommendationRecordOutput,
> = ToolDef::new(ToolInfo {
    name: "podcast_recommendation_record",
    title: "Record Podcast Recommendation",
    description: "Preview, reserve and settle one podcast episode recommendation that an assistant presents in chat. Every action defaults to dryRun true, which writes nothing. prepare verifies the exact episode in the show's RSS feed by GUID (and enclosure when given), checks subscriptions, listen history, the queue and recorded recommendations, and looks up the podcast client's episode read-only; enqueue lists the exact podcast_account_update enqueue arguments only when that mapping is verified by GUID or enclosure. With dryRun false prepare reserves a private pending record, idempotent per idempotencyKey, and at most one reservation per episode across all callers. finish records the chat send receipt (proof of sending, not of reading) or an unknown send, which stays pending and excluded until a later receipt finishes it; it is never resent and can never be withdrawn. withdraw fails only a reserved, never-sent reservation the caller knows was never presented, keeping the 24-hour retry exclusion; its idempotencyKey then replays the withdrawn record, so a new attempt needs a new key. This never queues, subscribes or sends a notification.",
    annotations: Annotations {
        read_only_hint: false,
        destructive_hint: false,
        idempotent_hint: true,
        open_world_hint: true,
    },
    policy: Policy {
        side_effects: &[
            "Reads the show's public RSS feed and the configured podcast account",
            "With dryRun false, writes only Omni's private recommendation record",
        ],
        cost: "No monetary cost; one feed fetch and a few podcast account reads",
        recommended: ApprovalPolicy::Allow,
    },
});

pub static PODCAST_TASTE_READ: ToolDef<PodcastTasteReadInput, PodcastTasteReadOutput> =
    ToolDef::new(ToolInfo {
        name: "podcast_taste_read",
        title: "Read Podcast Taste Data",
        description: "Read the latest derived podcast taste profile or paginated evidence supporting it.",
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

pub static PODCAST_TRANSCRIPT_SEARCH: ToolDef<
    PodcastTranscriptSearchInput,
    PodcastTranscriptSearchOutput,
> = ToolDef::new(ToolInfo {
    name: "podcast_transcript_search",
    title: "Search Podcast Transcripts",
    description: "Search Exa's podcast index (full episode transcripts plus show pages) for what was said, by topic, guest or phrase. Results are only exa.ai/library/podcast pages: kind is episode or show, and showId/episodeId come from the page URL. publishedDate is the publication date Exa reports for the page, passed through unchanged (Exa reports no crawl date). highlights are passages Exa selected as relevant, verbatim and clipped to highlightChars; they are not the full transcript and may lack speaker labels. Pass a result url to podcast_transcript_get for the full text, show RSS and platform links. Each call is one paid Exa request.",
    annotations: Annotations {
        read_only_hint: true,
        destructive_hint: false,
        idempotent_hint: true,
        open_world_hint: true,
    },
    policy: Policy {
        side_effects: &["Sends the query to Exa's search API"],
        cost: "One paid Exa search request per call, recorded in Omni costs at Exa's reported price",
        recommended: ApprovalPolicy::Allow,
    },
});

pub static PODCAST_TRANSCRIPT_GET: ToolDef<PodcastTranscriptGetInput, PodcastTranscriptGetOutput> =
    ToolDef::new(ToolInfo {
        name: "podcast_transcript_get",
        title: "Get Podcast Transcript",
        description: "Read one Exa podcast episode or show page by its exa.ai/library/podcast URL, paged by character offset. Episode pages carry the transcript as Exa formats it (speaker names and timestamps appear only where Exa's text has them; Omni adds and infers none). Show pages carry the description, hosts, publisher and Exa's list of recent episodes; links are the page links Exa extracted (show RSS feed and platform links when present), unverified. Omni retrieves at most 1,000,000 characters per page (sourceTruncated marks more) and caches the page for 15 minutes, so paging with nextOffset makes no new Exa request. status unavailable means Exa returned no usable text, with the reason; it is not evidence that the episode lacks a transcript elsewhere.",
        annotations: Annotations {
            read_only_hint: true,
            destructive_hint: false,
            idempotent_hint: true,
            open_world_hint: true,
        },
        policy: Policy {
            side_effects: &["Reads one podcast page through Exa's contents API"],
            cost: "One paid Exa contents request per uncached page, recorded in Omni costs at Exa's reported price",
            recommended: ApprovalPolicy::Allow,
        },
    });

/// Every podcast tool, in serving order.
pub static TOOLS: [&dyn ToolDefinition; 10] = [
    &PODCAST_ACCOUNT_LIST,
    &PODCAST_ACCOUNT_SEARCH,
    &PODCAST_ACCOUNT_UPDATE,
    &PODCAST_RECOMMENDATIONS_LIST,
    &PODCAST_RECOMMENDATION_GET,
    &PODCAST_RECOMMENDATION_FEEDBACK,
    &PODCAST_RECOMMENDATION_RECORD,
    &PODCAST_TASTE_READ,
    &PODCAST_TRANSCRIPT_SEARCH,
    &PODCAST_TRANSCRIPT_GET,
];

#[derive(JsonSchema)]
#[schemars(rename_all = "snake_case")]
pub enum Resource {
    Subscriptions,
    Queue,
    Inbox,
    ListenHistory,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct PodcastAccountListInput {
    pub resource: Resource,
    #[schemars(range(min = 1, max = 180), extend("default" = 30))]
    pub since_days: Option<u64>,
    #[schemars(length(max = 200))]
    pub query: Option<String>,
    #[schemars(description = "Zero-based result offset", extend("default" = 0))]
    pub cursor: Option<u64>,
    #[schemars(range(min = 1, max = 100), extend("default" = 25))]
    pub limit: Option<u64>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct PodcastAccountListOutputSubscriptionsItem {
    pub title: String,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub feed_url: Option<String>,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub itunes_id: Option<i64>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct PodcastAccountListOutputSubscriptions {
    pub next_cursor: Option<u64>,
    pub total: u64,
    pub account: String,
    #[schemars(transform = Literal("subscriptions"))]
    pub resource: Lit,
    pub items: Vec<PodcastAccountListOutputSubscriptionsItem>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct PodcastAccountListOutputQueueItem {
    pub show_title: String,
    pub episode_title: String,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub episode_guid: Option<String>,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub feed_url: Option<String>,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub added_at: Option<i64>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct PodcastAccountListOutputQueue {
    pub next_cursor: Option<u64>,
    pub total: u64,
    pub account: String,
    #[schemars(transform = Literal("queue"))]
    pub resource: Lit,
    pub items: Vec<PodcastAccountListOutputQueueItem>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct PodcastAccountListOutputInboxItem {
    pub client_episode_id: String,
    pub show_title: String,
    pub episode_title: String,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub episode_guid: Option<String>,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct PodcastAccountListOutputInbox {
    pub next_cursor: Option<u64>,
    pub total: u64,
    pub account: String,
    #[schemars(transform = Literal("inbox"))]
    pub resource: Lit,
    pub items: Vec<PodcastAccountListOutputInboxItem>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct PodcastAccountListOutputListenHistoryItem {
    pub show_title: String,
    pub episode_title: String,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub episode_guid: Option<String>,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub feed_url: Option<String>,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub itunes_id: Option<i64>,
    pub listened_at: i64,
    #[schemars(skip_serializing_if = "Option::is_none", range(min = 0, max = 1))]
    pub completion: Option<f64>,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub starred: Option<bool>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct PodcastAccountListOutputListenHistory {
    pub next_cursor: Option<u64>,
    pub total: u64,
    pub account: String,
    #[schemars(transform = Literal("listen_history"))]
    pub resource: Lit,
    pub items: Vec<PodcastAccountListOutputListenHistoryItem>,
}

#[derive(JsonSchema)]
#[schemars(untagged, transform = one_of)]
pub enum PodcastAccountListOutput {
    Subscriptions(PodcastAccountListOutputSubscriptions),
    Queue(PodcastAccountListOutputQueue),
    Inbox(PodcastAccountListOutputInbox),
    ListenHistory(PodcastAccountListOutputListenHistory),
}

#[derive(JsonSchema)]
#[schemars(rename_all = "lowercase")]
pub enum PodcastAccountSearchResource {
    Shows,
    Episodes,
}

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct PodcastAccountSearchInput {
    pub resource: PodcastAccountSearchResource,
    #[schemars(length(min = 1, max = 200))]
    pub query: String,
    #[schemars(description = "Zero-based result offset", extend("default" = 0))]
    pub cursor: Option<u64>,
    #[schemars(range(min = 1, max = 50), extend("default" = 20))]
    pub limit: Option<u64>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct PodcastAccountSearchOutputShowsItem {
    pub client_id: String,
    pub title: String,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    pub feed_url: String,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub itunes_id: Option<i64>,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub artwork_url: Option<String>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct PodcastAccountSearchOutputShows {
    pub next_cursor: Option<u64>,
    pub total: u64,
    pub account: String,
    #[schemars(transform = Literal("shows"))]
    pub resource: Lit,
    pub items: Vec<PodcastAccountSearchOutputShowsItem>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct PodcastAccountSearchOutputEpisodesItem {
    pub client_id: String,
    pub title: String,
    pub show_title: String,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub published_at: Option<i64>,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub artwork_url: Option<String>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct PodcastAccountSearchOutputEpisodes {
    pub next_cursor: Option<u64>,
    pub total: u64,
    pub account: String,
    #[schemars(transform = Literal("episodes"))]
    pub resource: Lit,
    pub items: Vec<PodcastAccountSearchOutputEpisodesItem>,
}

#[derive(JsonSchema)]
#[schemars(untagged, transform = one_of)]
pub enum PodcastAccountSearchOutput {
    Shows(PodcastAccountSearchOutputShows),
    Episodes(PodcastAccountSearchOutputEpisodes),
}

#[derive(JsonSchema)]
#[schemars(rename_all = "lowercase")]
pub enum Position {
    Next,
    Last,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase")]
pub struct PodcastAccountUpdateInputEnqueue {
    #[schemars(transform = Literal("enqueue"))]
    pub action: Lit,
    #[schemars(url)]
    pub feed_url: String,
    #[schemars(transform = positive)]
    pub itunes_id: Option<u64>,
    #[schemars(length(min = 1, max = 1000))]
    pub episode_guid: String,
    #[schemars(url)]
    pub media_url: Option<String>,
    #[schemars(length(min = 1, max = 300))]
    pub show_title: String,
    #[schemars(length(min = 1, max = 500))]
    pub episode_title: String,
    #[schemars(extend("default" = "next"))]
    pub position: Option<Position>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase")]
pub struct PodcastAccountUpdateInputDequeue {
    #[schemars(transform = Literal("dequeue"))]
    pub action: Lit,
    #[schemars(length(min = 1, max = 1000))]
    pub episode_guid: String,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase")]
pub struct PodcastAccountUpdateInputClearInbox {
    #[schemars(transform = Literal("clear_inbox"))]
    pub action: Lit,
    #[schemars(length(min = 1, max = 500))]
    pub client_episode_id: String,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase")]
pub struct PodcastAccountUpdateInputSubscribe {
    #[schemars(transform = Literal("subscribe"))]
    pub action: Lit,
    #[schemars(length(min = 1, max = 300))]
    pub title: String,
    #[schemars(url)]
    pub feed_url: String,
    #[schemars(transform = positive)]
    pub itunes_id: Option<u64>,
}

#[derive(JsonSchema)]
#[schemars(untagged, transform = one_of)]
pub enum PodcastAccountUpdateInput {
    Enqueue(PodcastAccountUpdateInputEnqueue),
    Dequeue(PodcastAccountUpdateInputDequeue),
    ClearInbox(PodcastAccountUpdateInputClearInbox),
    Subscribe(PodcastAccountUpdateInputSubscribe),
}

#[derive(JsonSchema)]
#[schemars(rename_all = "snake_case")]
pub enum Action {
    Enqueue,
    Dequeue,
    ClearInbox,
    Subscribe,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "snake_case")]
pub enum PodcastAccountUpdateResult {
    Added,
    Removed,
    AlreadyExists,
    NotFound,
    Unavailable,
    Error,
}

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct PodcastAccountUpdateOutput {
    pub account: String,
    pub action: Action,
    pub result: PodcastAccountUpdateResult,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "lowercase")]
pub enum Status {
    Pending,
    Notified,
    Listened,
    Abandoned,
    Ignored,
    Failed,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "snake_case")]
pub enum Feedback {
    GoodPick,
    NotForMe,
    None,
}

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct PodcastRecommendationsListInput {
    pub status: Option<Status>,
    pub feedback: Option<Feedback>,
    #[schemars(description = "Zero-based result offset", extend("default" = 0))]
    pub cursor: Option<u64>,
    #[schemars(range(min = 1, max = 100), extend("default" = 25))]
    pub limit: Option<u64>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "snake_case")]
pub enum PodcastRecommendationsListItemFeedback {
    GoodPick,
    NotForMe,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct PodcastRecommendationsListItem {
    pub recommendation_id: String,
    pub episode_id: String,
    pub show_id: String,
    pub show_title: String,
    pub episode_title: String,
    #[schemars(url)]
    pub feed_url: String,
    pub itunes_id: Option<i64>,
    pub episode_guid: String,
    pub media_url: Option<String>,
    pub episode_url: Option<String>,
    pub published_at: i64,
    pub duration_minutes: Option<f64>,
    pub status: Status,
    pub why_for_user: Option<String>,
    pub caveats: Vec<String>,
    pub confidence: Option<f64>,
    pub discovered_via: Option<String>,
    pub matched_voices: Vec<String>,
    pub recommended_at: i64,
    pub notified_at: Option<f64>,
    pub resolved_at: Option<f64>,
    pub queue_result: Option<String>,
    pub feedback: Option<PodcastRecommendationsListItemFeedback>,
    pub feedback_at: Option<f64>,
    pub feedback_note: Option<String>,
    pub origin: RecommendationOrigin,
    #[schemars(
        description = "Delivery bookkeeping of an assistant_chat recommendation; null for legacy_pipeline rows"
    )]
    pub chat: Option<ChatRecordView>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "snake_case")]
pub enum RecommendationOrigin {
    LegacyPipeline,
    AssistantChat,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "snake_case")]
pub enum ChatDelivery {
    Reserved,
    Sent,
    Unknown,
    Withdrawn,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChatRecordView {
    #[schemars(description = "Pass as expectedRevision to finish or withdraw")]
    pub revision: i64,
    #[schemars(
        description = "sent means the caller supplied a send receipt (not proof of reading); unknown stays pending until the caller reconciles it"
    )]
    pub delivery: ChatDelivery,
    pub updated_at: i64,
    pub channel: Option<String>,
    pub message_ref: Option<String>,
    pub sent_at: Option<i64>,
    pub provenance: Option<String>,
    pub unknown_reason: Option<String>,
    pub withdrawn_reason: Option<String>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct PodcastRecommendationsListOutput {
    pub next_cursor: Option<u64>,
    pub total: u64,
    pub items: Vec<PodcastRecommendationsListItem>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct PodcastRecommendationGetInput {
    #[schemars(length(min = 1, max = 300))]
    pub recommendation_id: String,
}

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct PodcastRecommendationGetOutput {
    pub recommendation: PodcastRecommendationsListItem,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct PodcastRecommendationFeedbackInput {
    #[schemars(length(min = 1, max = 300))]
    pub recommendation_id: String,
    pub feedback: Option<PodcastRecommendationsListItemFeedback>,
    #[schemars(length(max = 1000))]
    pub note: Option<String>,
}

pub type PodcastRecommendationFeedbackOutput = PodcastRecommendationGetOutput;

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct PodcastRecommendationRecordInputPrepare {
    #[schemars(transform = Literal("prepare"))]
    pub action: Lit,
    #[schemars(
        description = "true previews eligibility and writes nothing; false reserves a private pending record",
        extend("default" = true)
    )]
    pub dry_run: Option<bool>,
    #[schemars(
        description = "Stable per intended recommendation; a retry with the same key returns the same record",
        length(min = 1, max = 200)
    )]
    pub idempotency_key: String,
    #[schemars(description = "The show's own RSS feed URL", url)]
    pub feed_url: String,
    #[schemars(
        description = "The episode's exact RSS item GUID",
        length(min = 1, max = 1000)
    )]
    pub episode_guid: String,
    #[schemars(
        description = "The episode's enclosure URL; when given it must match the feed item",
        url
    )]
    pub enclosure_url: Option<String>,
    #[schemars(
        description = "The show's Apple Podcasts id, when known; it becomes part of the canonical show id",
        transform = positive
    )]
    pub itunes_id: Option<u64>,
    #[schemars(description = "Display title of the show", length(min = 1, max = 300))]
    pub show_title: String,
    #[schemars(
        description = "Why this episode fits, as presented to the owner",
        length(min = 1, max = 2000)
    )]
    pub why_for_user: String,
    #[schemars(length(max = 5))]
    pub caveats: Option<Vec<RecordCaveat>>,
}

#[derive(JsonSchema)]
pub struct RecordCaveat(#[schemars(length(min = 1, max = 300))] pub String);

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeliverySent {
    #[schemars(transform = Literal("sent"))]
    pub state: Lit,
    #[schemars(
        description = "The chat surface the message was sent on",
        length(min = 1, max = 100)
    )]
    pub channel: String,
    #[schemars(
        description = "The sent message's id on that channel",
        length(min = 1, max = 300)
    )]
    pub message_ref: String,
    #[schemars(description = "Epoch ms the channel reported for the send")]
    pub sent_at: i64,
    #[schemars(
        description = "The send call or tool result that produced this receipt",
        length(min = 1, max = 500)
    )]
    pub provenance: String,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeliveryUnknown {
    #[schemars(transform = Literal("unknown"))]
    pub state: Lit,
    #[schemars(length(min = 1, max = 500))]
    pub reason: String,
}

#[derive(JsonSchema)]
#[schemars(untagged, transform = one_of)]
pub enum DeliveryReport {
    Sent(DeliverySent),
    Unknown(DeliveryUnknown),
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct PodcastRecommendationRecordInputFinish {
    #[schemars(transform = Literal("finish"))]
    pub action: Lit,
    #[schemars(extend("default" = true))]
    pub dry_run: Option<bool>,
    #[schemars(length(min = 1, max = 300))]
    pub recommendation_id: String,
    #[schemars(description = "The record's current chat.revision")]
    pub expected_revision: i64,
    pub delivery: DeliveryReport,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct PodcastRecommendationRecordInputWithdraw {
    #[schemars(transform = Literal("withdraw"))]
    pub action: Lit,
    #[schemars(extend("default" = true))]
    pub dry_run: Option<bool>,
    #[schemars(length(min = 1, max = 300))]
    pub recommendation_id: String,
    pub expected_revision: i64,
    #[schemars(length(min = 1, max = 500))]
    pub reason: String,
    #[schemars(
        description = "Confirms the recommendation was definitely never presented",
        transform = Literal(true)
    )]
    pub not_presented: Lit,
}

#[derive(JsonSchema)]
#[schemars(untagged, transform = one_of)]
pub enum PodcastRecommendationRecordInput {
    Prepare(PodcastRecommendationRecordInputPrepare),
    Finish(PodcastRecommendationRecordInputFinish),
    Withdraw(PodcastRecommendationRecordInputWithdraw),
}

#[derive(JsonSchema)]
#[schemars(rename_all = "snake_case")]
pub enum RecordAction {
    Prepare,
    Finish,
    Withdraw,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "snake_case")]
pub enum RecordResult {
    Eligible,
    Ineligible,
    Unavailable,
    Reserved,
    Replayed,
    WouldChange,
    Changed,
    Unchanged,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct RecordIdentity {
    pub episode_id: String,
    pub show_id: String,
    pub show_title: String,
    pub episode_title: String,
    #[schemars(url)]
    pub feed_url: String,
    pub itunes_id: Option<i64>,
    pub episode_guid: String,
    pub media_url: Option<String>,
    pub episode_url: Option<String>,
    pub published_at: i64,
    pub duration_minutes: Option<i64>,
}

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct RecordExclusion {
    pub code: String,
    pub detail: String,
}

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct RecordUnavailable {
    pub source: String,
    pub reason: String,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "snake_case")]
pub enum ClientMappingStatus {
    Verified,
    Unverified,
    Unsupported,
    Unavailable,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "snake_case")]
pub enum MatchedBy {
    Guid,
    Enclosure,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClientMapping {
    pub status: ClientMappingStatus,
    pub client_episode_id: Option<String>,
    pub matched_by: Option<MatchedBy>,
    pub reason: Option<String>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnqueueArguments {
    #[schemars(url)]
    pub feed_url: String,
    pub itunes_id: Option<i64>,
    pub episode_guid: String,
    pub media_url: Option<String>,
    pub show_title: String,
    pub episode_title: String,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct PodcastRecommendationRecordOutput {
    pub action: RecordAction,
    pub dry_run: bool,
    #[schemars(
        description = "prepare: eligible (dry run), ineligible, unavailable (state unreadable), reserved or replayed. finish and withdraw: would_change (dry run), changed or unchanged (already recorded)"
    )]
    pub result: RecordResult,
    pub identity: Option<RecordIdentity>,
    pub exclusions: Vec<RecordExclusion>,
    pub unavailable: Vec<RecordUnavailable>,
    #[schemars(
        description = "The podcast client's episode, looked up read-only; null outside prepare"
    )]
    pub client: Option<ClientMapping>,
    #[schemars(
        description = "Arguments for podcast_account_update action enqueue, present only for a verified mapping; queueing needs the owner's approval"
    )]
    pub enqueue: Option<EnqueueArguments>,
    pub recommendation: Option<PodcastRecommendationsListItem>,
    #[schemars(description = "The owner's one-tap rating page for the recorded recommendation")]
    pub feedback_url: Option<String>,
}

#[derive(JsonSchema)]
pub struct PodcastTasteReadInputProfile {
    #[schemars(transform = Literal("profile"))]
    pub resource: Lit,
}

#[derive(JsonSchema)]
pub struct PodcastTasteReadInputEvidence {
    #[schemars(transform = Literal("evidence"))]
    pub resource: Lit,
    #[schemars(description = "Zero-based result offset", extend("default" = 0))]
    pub cursor: Option<u64>,
    #[schemars(range(min = 1, max = 100), extend("default" = 25))]
    pub limit: Option<u64>,
}

#[derive(JsonSchema)]
#[schemars(untagged, transform = one_of)]
pub enum PodcastTasteReadInput {
    Profile(PodcastTasteReadInputProfile),
    Evidence(PodcastTasteReadInputEvidence),
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct StablePreference {
    pub claim: String,
    pub confidence: f64,
    pub evidence_ids: Vec<String>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct Recommendations {
    pub total: u64,
    pub watched: u64,
    pub abandoned: u64,
    pub ignored: u64,
    pub failed: u64,
    pub awaiting_outcome: u64,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct StatsCompletedMoviesFeedback {
    pub good_pick: u64,
    pub not_for_me: u64,
    pub already_watched: u64,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct SourcePerformanceValue {
    pub total: u64,
    pub watched: u64,
    pub good_pick: u64,
    pub not_for_me: u64,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct StatsCompletedMovies {
    pub completed_movies: u64,
    pub completed_series: u64,
    pub rewatched_titles: u64,
    pub recommendations: Recommendations,
    pub feedback: StatsCompletedMoviesFeedback,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub average_hours_to_start: Option<f64>,
    pub source_performance: BTreeMap<String, SourcePerformanceValue>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct StatsListenedEpisodesRecommendations {
    pub total: u64,
    pub listened: u64,
    pub abandoned: u64,
    pub ignored: u64,
    pub failed: u64,
    pub awaiting_outcome: u64,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct StatsListenedEpisodesFeedback {
    pub good_pick: u64,
    pub not_for_me: u64,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct StatsListenedEpisodes {
    pub listened_episodes: u64,
    pub started_episodes: u64,
    pub starred_episodes: u64,
    pub distinct_shows: u64,
    pub recommendations: StatsListenedEpisodesRecommendations,
    pub feedback: StatsListenedEpisodesFeedback,
}

#[derive(JsonSchema)]
#[schemars(untagged)]
pub enum Stats {
    CompletedMovies(StatsCompletedMovies),
    ListenedEpisodes(StatsListenedEpisodes),
}

#[derive(JsonSchema)]
#[schemars(rename_all = "lowercase")]
pub enum Preference {
    Positive,
    Neutral,
    Negative,
    Uncertain,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct Movies {
    pub preference: Preference,
    pub confidence: f64,
    pub evidence_ids: Vec<String>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct CommitmentPreferences {
    pub movies: Movies,
    pub limited_series: Movies,
    pub long_series: Movies,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct Profile {
    pub profile_id: String,
    pub version: i64,
    pub generated_at: i64,
    pub evidence_fingerprint: String,
    pub evidence_count: u64,
    pub model_id: String,
    pub prompt_version: String,
    pub summary: String,
    pub stable_preferences: Vec<StablePreference>,
    pub conditional_preferences: Vec<StablePreference>,
    pub aversions: Vec<StablePreference>,
    pub current_saturation: Vec<StablePreference>,
    pub exploration_targets: Vec<StablePreference>,
    pub uncertainties: Vec<StablePreference>,
    pub stats: Stats,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub commitment_preferences: Option<CommitmentPreferences>,
}

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct PodcastTasteReadOutputProfile {
    #[schemars(transform = Literal("profile"))]
    pub resource: Lit,
    pub profile: Option<Profile>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "snake_case")]
pub enum Kind {
    Listen,
    RecommendationOutcome,
    ExplicitFeedback,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct PodcastTasteReadOutputEvidenceItem {
    pub evidence_id: String,
    pub kind: Kind,
    pub show_key: String,
    pub show_title: String,
    pub episode_title: Option<String>,
    pub observed_at: i64,
    pub completion: Option<f64>,
    pub recommendation_id: Option<String>,
    pub feedback: Option<PodcastRecommendationsListItemFeedback>,
    pub note: Option<String>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct PodcastTasteReadOutputEvidence {
    pub next_cursor: Option<u64>,
    pub total: u64,
    #[schemars(transform = Literal("evidence"))]
    pub resource: Lit,
    pub items: Vec<PodcastTasteReadOutputEvidenceItem>,
}

#[derive(JsonSchema)]
#[schemars(untagged, transform = one_of)]
pub enum PodcastTasteReadOutput {
    Profile(Box<PodcastTasteReadOutputProfile>),
    Evidence(PodcastTasteReadOutputEvidence),
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct PodcastTranscriptSearchInput {
    #[schemars(length(min = 1, max = 500))]
    pub query: String,
    #[schemars(range(min = 1, max = 10), extend("default" = 5))]
    pub limit: Option<u64>,
    #[schemars(
        description = "Only pages Exa dates on or after this (YYYY-MM-DD as midnight UTC, or RFC 3339)",
        length(min = 1, max = 40)
    )]
    pub published_after: Option<String>,
    #[schemars(
        description = "Only pages Exa dates on or before this (YYYY-MM-DD as midnight UTC, or RFC 3339)",
        length(min = 1, max = 40)
    )]
    pub published_before: Option<String>,
    #[schemars(
        description = "Maximum characters per highlight",
        range(min = 100, max = 2000),
        extend("default" = 600)
    )]
    pub highlight_chars: Option<u64>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "snake_case")]
pub enum PodcastPageKind {
    Show,
    Episode,
    Other,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct PodcastTranscriptSearchOutputResult {
    pub url: String,
    pub kind: PodcastPageKind,
    pub show_id: Option<String>,
    pub episode_id: Option<String>,
    pub exa_id: Option<String>,
    pub title: Option<String>,
    pub author: Option<String>,
    #[schemars(description = "Exa's publication date for the page, unchanged")]
    pub published_date: Option<String>,
    #[schemars(length(max = 3))]
    pub highlights: Vec<String>,
    pub highlights_truncated: bool,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct PodcastTranscriptSearchOutput {
    #[schemars(transform = Literal("exa.ai/library/podcast"))]
    pub index: Lit,
    pub request_id: Option<String>,
    pub results: Vec<PodcastTranscriptSearchOutputResult>,
    #[schemars(description = "Results outside the podcast index that Omni dropped")]
    pub dropped_results: u64,
    pub cost_dollars: Option<f64>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct PodcastTranscriptGetInput {
    #[schemars(
        description = "An Exa podcast page URL: https://exa.ai/library/podcast/<showId> or .../<showId>/episode/<episodeId>",
        length(min = 1, max = 300)
    )]
    pub url: String,
    #[schemars(
        description = "Zero-based character offset; pass the previous nextOffset",
        range(max = 1_000_000),
        extend("default" = 0)
    )]
    pub offset: Option<u64>,
    #[schemars(range(min = 1000, max = 40000), extend("default" = 20000))]
    pub max_chars: Option<u64>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "snake_case")]
pub enum PodcastPageKindExact {
    Show,
    Episode,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "snake_case")]
pub enum PodcastTranscriptUnavailableReason {
    ExaStatusError,
    NotReturned,
    EmptyText,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct PodcastTranscriptGetOutputAvailable {
    #[schemars(transform = Literal("available"))]
    pub status: Lit,
    pub url: String,
    pub kind: PodcastPageKindExact,
    pub show_id: String,
    pub episode_id: Option<String>,
    pub exa_id: Option<String>,
    pub title: Option<String>,
    pub author: Option<String>,
    #[schemars(description = "Exa's publication date for the page, unchanged")]
    pub published_date: Option<String>,
    #[schemars(description = "Exa's content source for this read: cached or crawled")]
    pub content_source: Option<String>,
    pub request_id: Option<String>,
    pub text: String,
    pub offset: u64,
    pub returned_chars: u64,
    pub total_chars: u64,
    pub next_offset: Option<u64>,
    #[schemars(description = "The page has text beyond the 1,000,000 characters Omni retrieves")]
    pub source_truncated: bool,
    #[schemars(length(max = 50))]
    pub links: Vec<String>,
    pub links_truncated: bool,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct PodcastTranscriptGetOutputUnavailable {
    #[schemars(transform = Literal("unavailable"))]
    pub status: Lit,
    pub url: String,
    pub kind: PodcastPageKindExact,
    pub show_id: String,
    pub episode_id: Option<String>,
    pub reason: PodcastTranscriptUnavailableReason,
    pub exa_error_tag: Option<String>,
    pub exa_http_status: Option<u16>,
    pub request_id: Option<String>,
}

#[derive(JsonSchema)]
#[schemars(untagged, transform = one_of)]
pub enum PodcastTranscriptGetOutput {
    Available(Box<PodcastTranscriptGetOutputAvailable>),
    Unavailable(PodcastTranscriptGetOutputUnavailable),
}
