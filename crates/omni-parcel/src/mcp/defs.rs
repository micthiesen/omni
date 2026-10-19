//! The Parcel tools' contracts: metadata, policy and the schema types their
//! input and output schemas derive from (`omni_mcp_kit::schema`). These types
//! describe the wire format only; the handlers decode and encode with their
//! own types. After changing anything here run `cargo xtask mcp-golden` and
//! review the snapshot diff.

use omni_mcp_kit::{Annotations, ApprovalPolicy, Policy, ToolDef, ToolDefinition, ToolInfo};
use schemars::JsonSchema;

const ANNOTATIONS: Annotations = Annotations {
    read_only_hint: true,
    destructive_hint: false,
    idempotent_hint: true,
    open_world_hint: false,
};

const POLICY: Policy = Policy {
    side_effects: &[],
    cost: "No external traffic or monetary cost; reads Omni's cached Parcel data",
    recommended: ApprovalPolicy::Allow,
};

pub static PARCELS_LIST: ToolDef<ParcelsListInput, ParcelsListOutput> = ToolDef::new(ToolInfo {
    name: "parcels_list",
    title: "List Parcel Deliveries",
    description: "List package deliveries tracked in the Parcel app with carrier, status, expected date and recent carrier events, plus the email Omni submitted each tracking number from. Data comes from Omni's cache, refreshed on a schedule (every 30 minutes while anything is active, every 3 hours otherwise); fetchedAt says how old it is and nothing here can force a refresh.",
    annotations: ANNOTATIONS,
    policy: POLICY,
});

pub static PARCELS_GET: ToolDef<ParcelsGetInput, ParcelsGetOutput> = ToolDef::new(ToolInfo {
    name: "parcels_get",
    title: "Get a Parcel Delivery",
    description: "Read one delivery from Omni's cached Parcel data by tracking number (spaces and case ignored), with up to 30 carrier events, newest first. Also reports whether Omni submitted that tracking number from an email, even when the cache does not include it yet.",
    annotations: ANNOTATIONS,
    policy: POLICY,
});

/// Every Parcel tool, in serving order.
pub static TOOLS: [&dyn ToolDefinition; 2] = [&PARCELS_LIST, &PARCELS_GET];

#[derive(JsonSchema)]
#[schemars(rename_all = "lowercase")]
pub enum Filter {
    Active,
    All,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct ParcelsListInput {
    #[schemars(
        description = "active (default) omits delivered packages",
        extend("default" = "active")
    )]
    pub filter: Option<Filter>,
    #[schemars(range(min = 1, max = 50), extend("default" = 20))]
    pub limit: Option<u64>,
    #[schemars(
        description = "Carrier events per delivery, newest first",
        range(max = 10),
        extend("default" = 3)
    )]
    pub events: Option<u64>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct ParcelsGetInput {
    #[schemars(length(min = 1, max = 200))]
    pub tracking_number: String,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "snake_case")]
pub enum Status {
    Completed,
    Frozen,
    InTransit,
    AwaitingPickup,
    OutForDelivery,
    NotFound,
    FailedAttempt,
    Exception,
    InfoReceived,
    Unknown,
}

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct Event {
    pub description: String,
    #[schemars(description = "Carrier text, verbatim; the format varies by carrier")]
    pub date: Option<String>,
    pub location: Option<String>,
    pub additional: Option<String>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct Source {
    pub email_id: String,
    pub activity_id: String,
    pub submitted_at: String,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct Delivery {
    pub tracking_number: String,
    pub carrier_code: String,
    pub carrier_name: Option<String>,
    pub description: String,
    pub status: Status,
    pub status_label: String,
    pub active: bool,
    #[schemars(description = "Parcel's expected delivery date, verbatim")]
    pub expected: Option<String>,
    pub expected_end: Option<String>,
    pub extra_information: Option<String>,
    pub events: Vec<Event>,
    #[schemars(description = "Events Parcel reported, before truncation")]
    pub event_count: u64,
    #[schemars(description = "The email Omni submitted this tracking number from")]
    pub source: Option<Source>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct CacheInfo {
    #[schemars(description = "False when Parcel is not configured; nothing is ever read")]
    pub configured: bool,
    #[schemars(description = "The last successful read; null before the first")]
    pub fetched_at: Option<String>,
    pub next_read_after: Option<String>,
    pub backoff_until: Option<String>,
    pub last_error: Option<String>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct ParcelsListOutput {
    pub cache: CacheInfo,
    pub active_count: u64,
    #[schemars(description = "Deliveries matching the filter, before the limit")]
    pub total: u64,
    pub deliveries: Vec<Delivery>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct ParcelsGetOutput {
    pub cache: CacheInfo,
    pub delivery: Option<Delivery>,
    #[schemars(description = "Omni's submission of this tracking number, when any")]
    pub submitted: Option<Source>,
}
